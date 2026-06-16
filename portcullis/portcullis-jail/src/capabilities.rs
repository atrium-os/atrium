//! Per-capability translators (spec §5).
//!
//! Each `apply_<cap>` is a small pure function that mutates the
//! `JailConfig` to grant that capability. Composable: `apply_all`
//! walks the manifest's `Capabilities` and dispatches.
//!
//! New capabilities are added by:
//!   1. Extending `Capabilities` in portcullis-toml/src/schema.rs.
//!   2. Adding an `apply_<cap>` function here.
//!   3. Calling it from `apply_all`.
//!   4. Adding a unit test.

use std::path::PathBuf;

use portcullis_toml::{Capabilities, NetworkCap};

use crate::{BuildError, BuildOpts, JailConfig, Value};

pub fn apply_all(
    caps: &Capabilities,
    jc: &mut JailConfig,
    opts: &BuildOpts,
) -> Result<(), BuildError> {
    if let Some(g) = &caps.graphics {
        apply_graphics(g, jc, opts)?;
    }
    if caps.clipboard == Some(true) {
        apply_socket("clipboard.sock", jc, opts);
    }
    if caps.notify == Some(true) {
        apply_socket("notify.sock", jc, opts);
    }
    if caps.open_uri == Some(true) {
        apply_socket("broker.sock", jc, opts);
    }
    if caps.audio == Some(true) {
        apply_audio(jc, opts);
    }
    if let Some(paths) = &caps.filesystem {
        for p in paths {
            apply_filesystem(p, jc, opts)?;
        }
    }
    if let Some(net) = caps.network {
        apply_network(net, jc);
    }
    if let Some(fonts) = &caps.fonts {
        for p in &fonts.paths {
            apply_fonts(p, &fonts.mode, jc, opts);
        }
    }
    if caps.tessera_cas_read == Some(true) {
        apply_tessera_cas_read(jc, opts);
    }
    if caps.usb_hid == Some(true) {
        apply_usb_hid(jc);
    }
    if caps.camera == Some(true) {
        apply_camera(jc);
    }
    if caps.microphone == Some(true) {
        apply_microphone(jc);
    }
    if caps.window_management == Some(true) {
        apply_window_management(jc, opts);
    }
    if caps.forum_control == Some(true) {
        apply_forum_control(jc, opts);
    }
    Ok(())
}

// ── individual translators ────────────────────────────────────

pub fn apply_graphics(value: &str, jc: &mut JailConfig, opts: &BuildOpts) -> Result<(), BuildError> {
    if value != "fresco" {
        return Err(BuildError::UnsupportedGraphics(value.to_string()));
    }
    apply_socket("fresco.sock", jc, opts);
    /* Fresco needs the GPU cdev too. */
    jc.add_devfs_action("path 'fresco0' unhide");
    Ok(())
}

/// `window-management` — the session shell (forum-wm). It connects to Fresco for
/// its own surfaces (declared separately via `graphics = "fresco"`) and, crucially,
/// *serves* the forum-ctl control socket that the chrome apps drive it through. The
/// jail role is to give it that socket path under the shared sockets dir; the actual
/// authority (mediating other apps' windows) is enforced service-side by frescod's
/// peer-cred → app-registry → policy check, not by the jail. See docs/spec/forum.md.
pub fn apply_window_management(jc: &mut JailConfig, opts: &BuildOpts) {
    apply_socket("forum-ctl.sock", jc, opts);
}

/// `forum-control` — a Forum chrome app (bar/dock/overview). It *connects* to the
/// forum-ctl socket forum-wm serves, to drive layout/focus. Same socket, client end.
/// Holding this cap never lets it touch another app's windows directly: it can only
/// ask forum-wm, which is the sole `window-management` holder.
pub fn apply_forum_control(jc: &mut JailConfig, opts: &BuildOpts) {
    apply_socket("forum-ctl.sock", jc, opts);
}

/// Grant a service socket into the jail.
///
/// FreeBSD's `mount_nullfs` refuses to mount a unix-socket node directly ("must be
/// either a file or directory"), so we cannot nullfs-mount the socket *file*.
/// Instead each service owns a DIRECTORY under the shared sockets root — e.g.
/// `/atrium/sockets/fresco/` holding `fresco.sock` — and we nullfs-mount that
/// per-service directory. This stays nullfs-legal AND preserves capability
/// granularity: an app gets only the directories for the caps it holds (a
/// `graphics`-only app never sees `audio/` or `forum-ctl/`). The socket itself,
/// visible through the mounted dir, is connectable from inside the jail (the
/// standard FreeBSD way a jail reaches a host service socket). The canonical
/// in-jail path is `/atrium/sockets/<service>/<service>.sock`
/// (see fresco-client's `default_socket_path`).
pub fn apply_socket(name: &str, jc: &mut JailConfig, opts: &BuildOpts) {
    let service = name.strip_suffix(".sock").unwrap_or(name);
    let src = opts.host_sockets.join(service);
    let dst_in_jail = jc.root_path.join("atrium/sockets").join(service);
    jc.add_mount(&src, &dst_in_jail, "nullfs", &["rw"]);
}

pub fn apply_audio(jc: &mut JailConfig, opts: &BuildOpts) {
    apply_socket("audio.sock", jc, opts);
    /* Audio data plane needs OSS device nodes. */
    jc.add_devfs_action("path 'dsp*' unhide");
    jc.add_devfs_action("path 'mixer*' unhide");
}

pub fn apply_filesystem(spec: &str, jc: &mut JailConfig, opts: &BuildOpts) -> Result<(), BuildError> {
    /* `~/foo` → opts.user_home/foo on host; → /home/<user>/foo in jail.
     * `/abs/path` → /abs/path on both sides. */
    let (host_path, in_jail) = if let Some(rest) = spec.strip_prefix("~/") {
        (opts.user_home.join(rest),
         PathBuf::from(format!("/home/{}", opts.user_name)).join(rest))
    } else if spec.starts_with('/') {
        (PathBuf::from(spec), PathBuf::from(spec))
    } else {
        return Err(BuildError::UnresolvedFilesystemPath(spec.to_string()));
    };
    let dst = jc.root_path.join(in_jail.strip_prefix("/").unwrap_or(&in_jail));
    jc.add_mount(&host_path, &dst, "nullfs", &["rw"]);
    Ok(())
}

pub fn apply_fonts(path: &str, mode: &str, jc: &mut JailConfig, _opts: &BuildOpts) {
    let src = PathBuf::from(path);
    let dst = jc.root_path.join(path.strip_prefix('/').unwrap_or(path));
    let opts = if mode == "read-only" { "ro" } else { "rw" };
    jc.add_mount(&src, &dst, "nullfs", &[opts]);
}

pub fn apply_network(net: NetworkCap, jc: &mut JailConfig) {
    /* FreeBSD jail.conf vnet legal values: new | inherit | disable.
     * For "no network" the canonical recipe is just disabling the
     * IP stacks; no vnet directive needed (default vnet=disable
     * means jail shares host's stack but ip4=disable / ip6=disable
     * leave it with no usable addresses). */
    match net {
        NetworkCap::None => {
            jc.set("ip4", Value::Symbolic("disable".into()));
            jc.set("ip6", Value::Symbolic("disable".into()));
            jc.set("allow.raw_sockets", Value::Bool(false));
        }
        NetworkCap::Loopback => {
            /* Per-jail loopback via fresh VNET — jail can't reach
             * host's other interfaces, only its own 127/8. */
            jc.set("vnet",     Value::Symbolic("new".into()));
            jc.set("ip4.addr", Value::String("127.0.0.1".into()));
            jc.set("ip6.addr", Value::String("::1".into()));
            jc.set("allow.raw_sockets", Value::Bool(false));
        }
        NetworkCap::Full => {
            /* Inherit host's VNET — jail sees real interfaces.
             * pf rules at the host enforce any further restriction. */
            jc.set("vnet", Value::Symbolic("inherit".into()));
            jc.set("allow.raw_sockets", Value::Bool(false));
        }
    }
}

pub fn apply_tessera_cas_read(jc: &mut JailConfig, _opts: &BuildOpts) {
    /* Trusted services only — gated by policy at portcullisd, not
     * here. Mount the host's Tessera CAS read-only. */
    let src = PathBuf::from("/var/lib/tessera/cas");
    let dst = jc.root_path.join("atrium/cas");
    jc.add_mount(&src, &dst, "nullfs", &["ro"]);
}

pub fn apply_usb_hid(jc: &mut JailConfig) {
    jc.add_devfs_action("path 'input/event*' unhide");
}

pub fn apply_camera(jc: &mut JailConfig) {
    jc.add_devfs_action("path 'video*' unhide");
}

pub fn apply_microphone(jc: &mut JailConfig) {
    /* Mic capture device — same OSS surface as audio output. */
    jc.add_devfs_action("path 'dsp*' unhide");
    jc.add_devfs_action("path 'mixer*' unhide");
}

// ── tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BuildOpts {
        BuildOpts {
            root_path:    PathBuf::from("/var/lib/atrium/jails/test"),
            host_sockets: PathBuf::from("/atrium/sockets"),
            user_home:    PathBuf::from("/home/alice"),
            user_name:    "alice".into(),
            devfs_ruleset: 99,
        }
    }

    fn jc() -> JailConfig {
        JailConfig::new("test".into(), PathBuf::from("/var/lib/atrium/jails/test"))
    }

    #[test]
    fn graphics_fresco_mounts_socket_and_unhides_dev() {
        let mut j = jc();
        apply_graphics("fresco", &mut j, &opts()).unwrap();
        assert_eq!(j.mounts.len(), 1);
        // Per-service DIRECTORY mount (nullfs can't mount the socket node itself).
        assert_eq!(j.mounts[0].src,
            PathBuf::from("/atrium/sockets/fresco"));
        assert_eq!(j.mounts[0].dst,
            PathBuf::from("/var/lib/atrium/jails/test/atrium/sockets/fresco"));
        assert_eq!(j.devfs_actions.len(), 1);
        assert!(j.devfs_actions[0].line.contains("fresco0"));
    }

    #[test]
    fn graphics_bad_value_rejected() {
        let mut j = jc();
        let err = apply_graphics("vulkan", &mut j, &opts()).unwrap_err();
        assert!(matches!(err, BuildError::UnsupportedGraphics(_)));
    }

    #[test]
    fn window_management_mounts_forum_ctl_socket() {
        let mut j = jc();
        apply_window_management(&mut j, &opts());
        assert_eq!(j.mounts.len(), 1);
        assert_eq!(j.mounts[0].src, PathBuf::from("/atrium/sockets/forum-ctl"));
        assert_eq!(j.mounts[0].dst,
            PathBuf::from("/var/lib/atrium/jails/test/atrium/sockets/forum-ctl"));
    }

    #[test]
    fn forum_control_mounts_forum_ctl_socket() {
        let mut j = jc();
        apply_forum_control(&mut j, &opts());
        assert_eq!(j.mounts.len(), 1);
        assert_eq!(j.mounts[0].src, PathBuf::from("/atrium/sockets/forum-ctl"));
    }

    #[test]
    fn clipboard_mounts_one_socket() {
        let mut j = jc();
        apply_socket("clipboard.sock", &mut j, &opts());
        assert_eq!(j.mounts.len(), 1);
        assert_eq!(j.mounts[0].src, PathBuf::from("/atrium/sockets/clipboard"));
    }

    #[test]
    fn filesystem_home_relative_resolves() {
        let mut j = jc();
        apply_filesystem("~/Documents", &mut j, &opts()).unwrap();
        assert_eq!(j.mounts[0].src, PathBuf::from("/home/alice/Documents"));
        assert!(j.mounts[0].dst.ends_with("home/alice/Documents"));
    }

    #[test]
    fn filesystem_absolute_passes_through() {
        let mut j = jc();
        apply_filesystem("/usr/share/myapp", &mut j, &opts()).unwrap();
        assert_eq!(j.mounts[0].src, PathBuf::from("/usr/share/myapp"));
        assert!(j.mounts[0].dst.ends_with("usr/share/myapp"));
    }

    #[test]
    fn network_none_disables_ip_no_vnet() {
        let mut j = jc();
        apply_network(NetworkCap::None, &mut j);
        assert!(j.has_set("ip4"));
        assert!(j.has_set("ip6"));
        /* vnet directive intentionally absent — disabling ip4/ip6
         * is the FreeBSD-canonical no-network recipe. */
        assert!(!j.has_set("vnet"));
    }

    #[test]
    fn network_loopback_creates_vnet_and_assigns_loopback() {
        let mut j = jc();
        apply_network(NetworkCap::Loopback, &mut j);
        let ip4 = j.params.iter().find(|(k, _)| k == "ip4.addr").unwrap();
        assert!(matches!(&ip4.1, Value::String(s) if s == "127.0.0.1"));
    }

    #[test]
    fn network_full_inherits_host_vnet() {
        let mut j = jc();
        apply_network(NetworkCap::Full, &mut j);
        let vnet = j.params.iter().find(|(k, _)| k == "vnet").unwrap();
        assert!(matches!(&vnet.1, Value::Symbolic(s) if s == "inherit"));
    }

    #[test]
    fn fonts_readonly() {
        let mut j = jc();
        apply_fonts("/usr/share/fonts", "read-only", &mut j, &opts());
        assert_eq!(j.mounts[0].opts, vec!["ro"]);
    }

    #[test]
    fn camera_unhides_video_devnode() {
        let mut j = jc();
        apply_camera(&mut j);
        assert!(j.devfs_actions[0].line.contains("video"));
    }
}
