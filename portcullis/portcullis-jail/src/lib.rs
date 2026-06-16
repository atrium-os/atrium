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
    /// devfs ruleset id assigned to this jail. Caller manages
    /// allocation across all jails on the host.
    pub devfs_ruleset: u32,
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
    let jail_name = jail_name_from_app_id(&manifest.app.id);
    let mut jc = JailConfig::new(jail_name, opts.root_path.clone());

    /* Defaults every Atrium jail wants. */
    jc.set("host.hostname", Value::String(manifest.app.id.clone()));
    jc.set("persist",       Value::Bool(true));
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
    jc.set("exec.start", Value::String(format!("/{}", manifest.app.entry)));

    /* Apply each declared capability. */
    capabilities::apply_all(&manifest.capabilities, &mut jc, opts)?;

    /* Network defaults to "none" if no capability set it. */
    if !jc.has_set("ip4") && !jc.has_set("vnet") {
        capabilities::apply_network(portcullis_toml::NetworkCap::None, &mut jc);
    }

    Ok(jc)
}
