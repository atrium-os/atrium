//! portcullis-jail — translate an atrium.toml `Manifest` into a
//! jail.conf section + mount + devfs ruleset.
//!
//! Pure Rust. Does NOT invoke `jail(8)` — that's the CLI's
//! responsibility (so this crate stays unit-testable on macOS host).
//!
//! See `docs/spec/portcullis.md` §5 for the per-capability
//! translation table this implements.

pub mod capabilities;
pub mod config;
pub mod render;

use std::path::PathBuf;

use thiserror::Error;

use portcullis_toml::Manifest;

pub use config::{JailConfig, MountSpec, Value};

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("unsupported graphics value: {0:?}")]
    UnsupportedGraphics(String),
    #[error("filesystem path {0:?} could not be resolved (check ~/ expansion)")]
    UnresolvedFilesystemPath(String),
    #[error("internal: {0}")]
    Internal(&'static str),
}

/// The devfs ruleset every portcullis app and one-shot jail is mounted with:
/// `atrium_app` in etc/atrium.devfs.rules — hide the host's /dev, unhide the
/// basics and the pty/fd set.
///
/// ★★★ This was the literal 99, "Phase 4 manages allocation", and no ruleset 99
/// was ever defined. A devfs mounted with an unloaded ruleset hides NOTHING, so
/// every app and every one-shot worker saw the host's entire /dev — raw disks,
/// mem/kmem, bpf (measured 2026-09-22; a root process in such a jail read the
/// host's disk). The number now names a ruleset that exists, and callers refuse
/// to create a jail if it is not loaded (`portcullis_mounts::ensure_devfs_isolation`, and jaild itself).
pub const APP_DEVFS_RULESET: u32 = 22;

/// Inputs the builder needs that aren't in the manifest.
pub struct BuildOpts {
    /// On-host path to the per-jail tree (rootfs union mount root).
    pub root_path:    PathBuf,
    /// On-host path where service sockets live (typically
    /// /atrium/sockets/). Capability mounts pull individual sockets
    /// from here.
    pub host_sockets: PathBuf,
    /// User home directory (for ~/-prefixed filesystem caps).
    pub user_home:    PathBuf,
    /// The name the app process RUNS AS — the dedicated, non-root per-app uid's
    /// account (not the human; see portcullis.md §9.0). Drives `exec.jail_user`.
    pub user_name:    String,
    /// devfs ruleset id this jail's /dev is mounted with — normally
    /// [`APP_DEVFS_RULESET`]. Capability device grants are NOT folded into it:
    /// `build` applies them to this jail's own devfs mount (see there), so one
    /// shared ruleset serves every app.
    pub devfs_ruleset: u32,
    /// ★★ AN INSTANCE TAG, for apps that run MORE THAN ONE JAIL AT A TIME.
    ///
    /// Jail names were derived from the app id alone, which silently assumes
    /// one live jail per app. That holds for a desktop application and fails
    /// completely for a worker pool: the Navigator's backend runs one jailed
    /// document worker PER DOCUMENT (atrium-navigator-backend.md §2, "one
    /// jail per document is the default, not a mitigation"), so sixteen open
    /// pages are sixteen concurrent jails of the same app. Without a
    /// per-instance name they collide on the jail name, and `jail -c` on an
    /// existing name reconfigures the running jail instead of creating one —
    /// two documents would end up sharing a jail, which is the exact property
    /// the design exists to prevent.
    ///
    /// `None` reproduces the single-instance name exactly, so nothing that
    /// launches an ordinary app changes.
    ///
    /// The HOSTNAME deliberately does not take the tag: it is what the app
    /// sees of itself, and a document worker should not be able to read which
    /// slot it was given.
    pub instance: Option<String>,
    /// ★★ WHETHER THE JAIL OUTLIVES ITS PROCESSES.
    ///
    /// `true` is right for an application: jail(8) must keep the jail while
    /// `exec.start` runs, and the launcher removes it afterwards.
    ///
    /// `false` is right for a unit of work, and fixes a whole class of
    /// wreckage rather than working around it. With `persist = true` a
    /// launcher that is KILLED never reaches its teardown, and the kernel
    /// keeps a named, process-less jail forever — a husk that poisons its
    /// instance tag for the next worker, and that the memory federation
    /// happily budgets and pins an rctl rule to. Both of those were found and
    /// patched around separately before the cause was addressed here. With
    /// `persist = false` the jail is removed the moment its last process
    /// exits, so killing the launcher cleans up by construction: the worker
    /// sees EOF on the pipe that died with its parent, exits, and the jail
    /// goes with it.
    pub persist: bool,
    /// ★★★ The host identity the jail reports — synthetic, per app, never the
    /// real machine's (portcullis_identity; portcullis.md §9.1c). REQUIRED, not
    /// defaulted: a launch path that forgot it would run apps with hostid 0 and
    /// a zero UUID, and making it a compile error is how every entrance is
    /// covered.
    pub host_identity: portcullis_identity::HostIdentity,
}

/// Jail name for one instance of an app.
///
/// ★ The tag is sanitized the same way the id is. A caller that passed a
/// dotted or slashed instance tag would otherwise produce a name jail(8)
/// reads as a HIERARCHY — `a.b` is a child jail of `a` — turning a naming
/// convenience into a nesting bug.
pub fn jail_name_for_instance(app_id: &str, instance: Option<&str>) -> String {
    let base = jail_name_from_app_id(app_id);
    match instance {
        None => base,
        Some(tag) => {
            let tag: String = tag.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!("{base}__{tag}")
        }
    }
}

/// The jaild-lane name for a one-shot instance: `app-<id>--<tag>`.
///
/// jaild accepts only `[a-z0-9-]`, a known prefix and at most 64 bytes
/// (jaild/src/validator.rs), and its one-shot rule trusts an `app-` jail whose
/// root is exactly `/var/lib/atrium/jails/<name>` — so the name IS the root's
/// last component, and must be jaild-valid by construction rather than
/// rejected at the socket. Every other character becomes `-`; the `--`
/// separates id from tag. `None` when the result would exceed jaild's limit —
/// refused, never truncated, because a truncated name could collide with
/// another app's.
///
/// ★ Two ids that differ only in punctuation (`org.a-b`, `org.a.b`) map to one
/// name. A collision is refused as "already running" at creation, never merged.
pub fn jaild_instance_name(app_id: &str, instance: Option<&str>) -> Option<String> {
    let clean = |s: &str| -> String {
        s.chars().map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }
        }).collect()
    };
    let name = format!("app-{}--{}", clean(app_id), clean(instance.unwrap_or("0")));
    (name.len() <= 64).then_some(name)
}

/// FreeBSD jail names use dots as hierarchy separators. Atrium app
/// IDs are reverse-DNS-style and contain dots. Sanitize by replacing
/// dots with underscores. The hostname keeps the original id.
pub fn jail_name_from_app_id(app_id: &str) -> String {
    app_id.replace('.', "_")
}

/// Build a JailConfig from a parsed manifest. Pure transformation;
/// no I/O.
pub fn build(manifest: &Manifest, opts: &BuildOpts) -> Result<JailConfig, BuildError> {
    let jail_name = jail_name_for_instance(&manifest.app.id, opts.instance.as_deref());
    let mut jc = JailConfig::new(jail_name, opts.root_path.clone());

    /* Defaults every Atrium jail wants. */
    jc.set("host.hostname", Value::String(manifest.app.id.clone()));
    jc.set("host.hostid",   Value::Number(opts.host_identity.hostid as i64));
    jc.set("host.hostuuid", Value::String(opts.host_identity.hostuuid.clone()));
    jc.set("persist",       Value::Bool(opts.persist));
    jc.set("mount.devfs",   Value::Bool(true));
    jc.set("devfs_ruleset", Value::Number(opts.devfs_ruleset as i64));
    jc.set("exec.clean",    Value::Bool(true));
    /* Atrium apps run as the calling user (host-managed identity),
     * not as a user with a passwd entry inside the jail. Tell jail(8)
     * to look up exec.jail_user from the host's /etc/passwd, not the
     * jail's. Without this, jails with no /etc/passwd in their tree
     * fail at jail-create time with "getpwnam: No such file or
     * directory".
     *
     * Polarity per jail.conf(5): exec.system_jail_user = true
     * looks in the SYSTEM passwd (host); false looks in the JAIL's
     * passwd (default for back-compat). We want the host lookup. */
    jc.set("exec.system_jail_user", Value::Bool(true));
    /* exec.jail_user names the user the entry runs as inside the
     * jail. Combined with exec.system_jail_user=true, jail(8) looks
     * the name up in the HOST's passwd to get the uid, and the entry
     * runs as that uid. portcullisd's per-user multi-tenancy passes
     * the connecting user through opts.user_name and lands here, so
     * an app launched by alice runs as uid(alice). */
    jc.set("exec.jail_user", Value::String(opts.user_name.clone()));
    /* exec.start runs the app's entry. Inside the jail, rootfs is
     * mounted at /, so the manifest's relative entry path becomes
     * /<entry> in the jail's namespace.
     *
     * Apps that want rc.d helpers ship a wrapper script as their
     * entry that does `/etc/rc; exec /usr/local/bin/myapp`. We
     * don't force /etc/rc on every jail — minimal app trees with
     * no /etc/rc would fail to launch. (Spec §3.5 documents the
     * patterns.) */
    jc.set("exec.start", Value::String(format!("/{}", manifest.entry())));

    /* Apply each declared capability. */
    capabilities::apply_all(&manifest.capabilities, &mut jc, opts)?;

    /* ★★ Capability device grants, applied to THIS jail's devfs mount.
     *
     * Capabilities have always recorded devfs actions (audio unhides dsp*,
     * input unhides input/event*, …) and nothing ever applied them: the
     * per-jail rules were rendered by render_devfs_rules and never loaded, and
     * the ruleset the jail was mounted with (99) did not exist. Apps got their
     * devices only because nothing was hidden at all. With a real baseline
     * ruleset those grants must actually happen, per jail.
     *
     * `devfs -m <mnt> rule apply <rule>` applies one rule to one mount without
     * adding it to any ruleset, so no ruleset numbers are allocated per app.
     * exec.prestart, because jail(8) mounts devfs BEFORE prestart and creates
     * the jail AFTER it (usr.sbin/jail/jail.c, create sequence): the grants are
     * in place before anything can run inside. `&&` so a rule that fails to
     * apply fails the launch rather than starting an app without its device. */
    if !jc.devfs_actions.is_empty() {
        let dev = opts.root_path.join("dev");
        let cmds: Vec<String> = jc.devfs_actions.iter()
            .map(|a| format!("devfs -m {} rule apply {}", dev.display(), a.line))
            .collect();
        jc.set("exec.prestart", Value::String(cmds.join(" && ")));
    }

    /* Network defaults to "none" if no capability set it. */
    if !jc.has_set("vnet") {
        capabilities::apply_network(portcullis_toml::NetworkCap::None, &mut jc);
    }

    Ok(jc)
}
