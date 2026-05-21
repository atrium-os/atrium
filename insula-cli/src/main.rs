//! `insula` — command-line frontend to the Insula host
//! adapter on macOS.
//!
//! ```text
//! Usage: insula <command> [args...]
//!
//! Commands:
//!   install <bundle-dir>    Install an Insula bundle from disk.
//!   list                    Show installed apps.
//!   info <app-id>           Show details for one installed app.
//!   launch <app-id> [args]  Launch an installed app (inherits stdio).
//!   uninstall <app-id>      Remove an installed app + its container.
//!   help                    Show this help.
//!
//! Install root defaults to
//!   ~/Library/Application Support/atrium-insula/
//! and can be overridden with $INSULA_INSTALL_ROOT.
//! ```

mod daemons;
mod push;
mod signing;
use daemons::Daemon;

use insula_bundle::{archive, InsulaBundle};
use insula_host_macos as host;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        return ExitCode::from(2);
    }

    let install_root = resolve_install_root();

    let result = match args[1].as_str() {
        "install" => cmd_install(&args[2..], &install_root),
        "list" => cmd_list(&install_root),
        "info" => cmd_info(&args[2..], &install_root),
        "launch" => cmd_launch(&args[2..], &install_root),
        "uninstall" => cmd_uninstall(&args[2..], &install_root),
        "daemons" => cmd_daemons(&args[2..], &install_root),
        "keygen" => signing::cmd_keygen(&args[2..]),
        "sign" => signing::cmd_sign(&args[2..]),
        "publishers" => signing::cmd_publishers(&args[2..], &install_root),
        "bundle" => cmd_bundle(&args[2..]),
        "push" => push::cmd_push(&args[2..], &install_root),
        "help" | "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("insula: unknown command: {}", other);
            print_usage();
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("insula: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "Usage: insula <command> [args...]

Commands:
  install <bundle-dir>    Install an Insula bundle from disk.
  list                    Show installed apps.
  info <app-id>           Show details for one installed app.
  launch <app-id> [args]  Launch an installed app (inherits stdio).
                          Auto-spawns missing daemons.
  uninstall <app-id>      Remove an installed app + its container.
  daemons up              Start all platform daemons.
  daemons down            Stop them.
  daemons status          Show daemon state.
  keygen <id> <out-dir>   Generate an ed25519 keypair for signing.
  sign <bundle> --key <f> [--key-id <id>]
                          Sign a bundle in place.
  publishers add <id> <pub-file>   Add a trusted publisher.
  publishers list                  Show trusted publishers.
  publishers remove <id>           Remove a trusted publisher.
  bundle <src-dir> <out.insula>    Pack a bundle directory into a
                                   single-file `.insula` archive.
  push subscribe <purpose>         Subscribe to push delivery.
  push list                        Show active push subscriptions.
  push unsubscribe <key_id>        Remove a push subscription.
  help                    Show this help.

Flags:
  --allow-unsigned        Pass to `install` to skip signature
                          verification (development only).
  --accept-changes        Pass to `install` to accept widened
                          capability grants on re-install.

Install root: $INSULA_INSTALL_ROOT (or ~/Library/Application Support/atrium-insula/)"
    );
}

fn resolve_install_root() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_INSTALL_ROOT") {
        return PathBuf::from(p);
    }

    // ~/Library/Application Support/atrium-insula/
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("atrium-insula");
    }

    // Last-ditch fallback.
    PathBuf::from("/tmp/atrium-insula")
}

// -----------------------------------------------------
// install
// -----------------------------------------------------

fn cmd_install(args: &[String], install_root: &Path) -> Result<(), String> {
    // Parse: <bundle-dir-or-archive> [--allow-unsigned] [--accept-changes]
    let mut src: Option<&str> = None;
    let mut allow_unsigned = false;
    let mut accept_changes = false;
    for a in args {
        match a.as_str() {
            "--allow-unsigned" => allow_unsigned = true,
            "--accept-changes" => accept_changes = true,
            other if !other.starts_with("--") => src = Some(other),
            other => {
                return Err(format!("install: unknown flag '{}'", other));
            }
        }
    }
    let src = src.ok_or_else(|| {
        "install: missing <bundle-dir|archive> argument".to_string()
    })?;

    // If the path looks like a .insula archive, extract
    // to a temp directory and install from there. The
    // _extract_guard's Drop removes the tempdir once
    // install completes (host::install copies the
    // bundle into the install root, so the tempdir's
    // lifetime only needs to cover this function).
    let _extract_guard: Option<TempDir>;
    let bundle_dir_path: PathBuf = if archive::path_looks_like_archive(Path::new(src)) {
        let tmp = TempDir::new("insula-extract")
            .map_err(|e| format!("create temp dir: {}", e))?;
        archive::unpack_into(src, tmp.path())
            .map_err(|e| format!("unpacking archive {}: {}", src, e))?;
        let p = tmp.path().to_path_buf();
        _extract_guard = Some(tmp);
        p
    } else {
        _extract_guard = None;
        PathBuf::from(src)
    };

    let bundle = InsulaBundle::read(&bundle_dir_path)
        .map_err(|e| format!("reading bundle at {}: {}", bundle_dir_path.display(), e))?;

    std::fs::create_dir_all(install_root)
        .map_err(|e| format!("create install root {}: {}", install_root.display(), e))?;

    // Signature verification.
    let sig_path = bundle.root.join("signature");
    if sig_path.exists() {
        let sig = signing::verify_bundle_signature(&bundle, install_root)
            .map_err(|e| format!("signature check: {}", e))?;
        println!(
            "signature verified (key_id = {})",
            sig.key_id
        );
    } else if allow_unsigned {
        eprintln!(
            "WARNING: installing unsigned bundle (--allow-unsigned set). \
             Don't use this in production."
        );
    } else {
        return Err(
            "bundle is unsigned. Sign it with `insula sign`, or pass \
             --allow-unsigned for dev installs."
                .to_string(),
        );
    }

    // Capability-diff consent on re-install. If the app
    // is already installed, compute the diff between
    // the old manifest and the new one. Widening grants
    // require --accept-changes to proceed.
    let existing_manifest_path = install_root
        .join("apps")
        .join(bundle.app_id())
        .join("bundle")
        .join("manifest.toml");
    if existing_manifest_path.is_file() {
        let old_src = std::fs::read_to_string(&existing_manifest_path)
            .map_err(|e| format!(
                "read existing manifest {}: {}",
                existing_manifest_path.display(), e
            ))?;
        let old_manifest = insula_manifest::Manifest::parse(&old_src)
            .map_err(|e| format!("parse existing manifest: {}", e))?;
        let diff = insula_manifest::CapabilityDiff::between(
            &old_manifest, &bundle.manifest,
        );
        if diff.is_widening() {
            if accept_changes {
                println!(
                    "accepting widened capabilities ({} -> {}):",
                    old_manifest.app.version, bundle.manifest.app.version,
                );
                println!("{}", diff.human_summary());
            } else {
                let mut msg = String::from(
                    "this re-install widens capabilities the user has not consented to.\n",
                );
                msg.push_str(&diff.human_summary());
                msg.push_str(
                    "\n\nPass --accept-changes to proceed, or uninstall the existing \
                     app first if you want a clean slate.",
                );
                return Err(msg);
            }
        }
    }

    let app = host::install(&bundle, install_root)
        .map_err(|e| format!("installing {}: {}", bundle.app_id(), e))?;

    println!("Installed {} v{}", app.app_id, app.manifest.app.version);
    println!("  bundle:    {}", app.binary_path.parent().unwrap().parent().unwrap().display());
    println!("  binary:    {}", app.binary_path.display());
    println!("  container: {}", app.container_dir.display());
    Ok(())
}

// -----------------------------------------------------
// list
// -----------------------------------------------------

fn cmd_list(install_root: &Path) -> Result<(), String> {
    let apps_dir = install_root.join("apps");
    if !apps_dir.is_dir() {
        println!("(no apps installed)");
        return Ok(());
    }

    let mut found = 0usize;
    for entry in std::fs::read_dir(&apps_dir)
        .map_err(|e| format!("read_dir {}: {}", apps_dir.display(), e))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let manifest_path = entry.path().join("bundle").join("manifest.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let src = match std::fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let manifest = match insula_manifest::Manifest::parse(&src) {
            Ok(m) => m,
            Err(_) => continue,
        };
        println!(
            "{}  {}  (entry: {})",
            manifest.app.name,
            manifest.app.version,
            manifest.bundle.entry
        );
        found += 1;
    }

    if found == 0 {
        println!("(no apps installed)");
    }
    Ok(())
}

// -----------------------------------------------------
// info
// -----------------------------------------------------

fn cmd_info(args: &[String], install_root: &Path) -> Result<(), String> {
    let app_id = args.first().ok_or_else(|| {
        "info: missing <app-id> argument".to_string()
    })?;

    let manifest_path = install_root.join("apps").join(app_id)
        .join("bundle").join("manifest.toml");
    let src = std::fs::read_to_string(&manifest_path)
        .map_err(|_| format!("app not installed: {}", app_id))?;
    let m = insula_manifest::Manifest::parse(&src)
        .map_err(|e| format!("parse manifest: {}", e))?;

    println!("{}", m.app.name);
    println!("  version:     {}", m.app.version);
    println!("  sdk version: {}", m.app.sdk_version);
    println!(
        "  bundle:      form={:?}, arches={:?}, entry={}",
        m.bundle.form, m.bundle.arches, m.bundle.entry
    );
    if let Some(r) = &m.render {
        println!("  render:      fresco={}", r.fresco);
    }
    if let Some(net) = &m.network {
        println!("  network:     {} host(s), raw-network={}",
                 net.hosts.len(), net.raw_network);
        for h in &net.hosts {
            println!("    - {} {}:{:?}", h.name, h.port, h.proto);
        }
    }
    if let Some(s) = &m.storage {
        println!("  storage:     data={:?}, cache={:?}",
                 s.data, s.cache);
    }
    if let Some(ipc) = &m.ipc {
        println!("  ipc services: {}", ipc.services.join(", "));
    }
    Ok(())
}

// -----------------------------------------------------
// launch
// -----------------------------------------------------

fn cmd_launch(args: &[String], install_root: &Path) -> Result<(), String> {
    let app_id = args.first().ok_or_else(|| {
        "launch: missing <app-id> argument".to_string()
    })?;
    let app_args_raw: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();

    let manifest_path = install_root.join("apps").join(app_id)
        .join("bundle").join("manifest.toml");
    let src = std::fs::read_to_string(&manifest_path)
        .map_err(|_| format!("app not installed: {}", app_id))?;
    let manifest = insula_manifest::Manifest::parse(&src)
        .map_err(|e| format!("parse manifest: {}", e))?;

    let binary_path = install_root.join("apps").join(app_id)
        .join("bundle").join(&manifest.bundle.entry);
    let container_dir = install_root.join("apps").join(app_id).join("container");

    let installed = host::InstalledApp {
        app_id: app_id.clone(),
        binary_path,
        container_dir,
        manifest,
    };

    // Daemon socket resolution:
    //   1. If an explicit env var is set, use that.
    //      (Tests + power users override this way.)
    //   2. Otherwise, auto-spawn the daemon under
    //      <install_root>/run/ and use that socket.
    //
    // The auto-spawn is best-effort — if the daemon
    // binary isn't on $PATH (and no override env var
    // is set), the launch still proceeds without the
    // service, just without log routing / keychain.
    let log_socket = resolve_daemon_socket(
        install_root,
        Daemon::Logd,
        "INSULA_LOGD_SOCKET",
    );
    let vestibulum_socket = resolve_daemon_socket(
        install_root,
        Daemon::Vestibulum,
        "INSULA_VESTIBULUMD_SOCKET",
    );
    let netd_socket = resolve_daemon_socket(
        install_root,
        Daemon::Netd,
        "INSULA_NETD_SOCKET",
    );
    let praeco_socket = resolve_daemon_socket(
        install_root,
        Daemon::Praeco,
        "INSULA_PRAECOD_SOCKET",
    );
    let tabellarius_socket = resolve_daemon_socket(
        install_root,
        Daemon::Tabellarius,
        "INSULA_TABELLARIUSD_SOCKET",
    );

    // Inherit stdio for `insula launch`; the user wants
    // to see the app's output.
    let mut child = host::launch_installed_v3(
        &installed,
        &app_args_raw,
        false,
        log_socket.as_deref(),
        vestibulum_socket.as_deref(),
        netd_socket.as_deref(),
        praeco_socket.as_deref(),
        tabellarius_socket.as_deref(),
    )
    .map_err(|e| format!("launch: {}", e))?;

    let status = child.child.wait()
        .map_err(|e| format!("wait: {}", e))?;

    // Propagate the child's exit code where possible.
    if !status.success() {
        if let Some(code) = status.code() {
            std::process::exit(code);
        } else {
            std::process::exit(128);
        }
    }
    Ok(())
}

// -----------------------------------------------------
// uninstall
// -----------------------------------------------------

// -----------------------------------------------------
// daemons + helpers
// -----------------------------------------------------

/// Resolve the socket path for one daemon: explicit
/// env-var override wins; otherwise auto-spawn under
/// install_root and use the resulting socket.
fn resolve_daemon_socket(
    install_root: &Path,
    daemon: Daemon,
    explicit_env: &str,
) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(explicit_env) {
        return Some(PathBuf::from(p));
    }
    match daemons::start(install_root, daemon) {
        Ok(_) => daemons::socket_if_running(install_root, daemon),
        Err(e) => {
            // Spawn failure is non-fatal — the app still
            // runs, just without this service.
            eprintln!(
                "insula: warning — could not auto-spawn {}: {} \
                 (proceeding without)",
                daemon.binary_name(),
                e
            );
            None
        }
    }
}

fn cmd_daemons(args: &[String], install_root: &Path) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    match sub {
        "up" | "start" => {
            for d in Daemon::ALL {
                match daemons::start(install_root, d) {
                    Ok(pid) => println!("{}: started (pid {})", d.slug(), pid),
                    Err(e) => println!("{}: ERROR {}", d.slug(), e),
                }
            }
        }
        "down" | "stop" => {
            for d in Daemon::ALL {
                daemons::stop(install_root, d)?;
                println!("{}: stopped", d.slug());
            }
        }
        "status" => {
            for d in Daemon::ALL {
                let (running, pid, sock) = daemons::status(install_root, d);
                println!(
                    "{}: {} pid={:?} socket={}",
                    d.slug(),
                    if running { "running" } else { "stopped" },
                    pid,
                    if sock { "ok" } else { "missing" }
                );
            }
        }
        other => {
            return Err(format!(
                "daemons: unknown subcommand '{}' (use up|down|status)",
                other
            ));
        }
    }
    Ok(())
}

fn cmd_uninstall(args: &[String], install_root: &Path) -> Result<(), String> {
    let app_id = args.first().ok_or_else(|| {
        "uninstall: missing <app-id> argument".to_string()
    })?;

    let app_root = install_root.join("apps").join(app_id);
    if !app_root.is_dir() {
        return Err(format!("app not installed: {}", app_id));
    }

    std::fs::remove_dir_all(&app_root)
        .map_err(|e| format!("removing {}: {}", app_root.display(), e))?;

    println!("Uninstalled {}", app_id);
    Ok(())
}

// -----------------------------------------------------
// bundle (pack a directory into a .insula archive)
// -----------------------------------------------------

fn cmd_bundle(args: &[String]) -> Result<(), String> {
    let src = args.first().ok_or_else(|| {
        "bundle: missing <src-dir> argument".to_string()
    })?;
    let out = args.get(1).ok_or_else(|| {
        "bundle: missing <out.insula> argument".to_string()
    })?;

    // Validate the directory is actually a bundle
    // before packing — fail fast on bad inputs.
    let bundle = InsulaBundle::read(src)
        .map_err(|e| format!("source {} is not a valid bundle: {}", src, e))?;

    archive::pack_dir(src, out)
        .map_err(|e| format!("packing {} -> {}: {}", src, out, e))?;

    println!("packed {} -> {}", bundle.app_id(), out);
    println!("  manifest:  {}/manifest.toml", src);
    println!("  entry:     {}", bundle.binary_path().display());
    if Path::new(src).join("signature").exists() {
        println!("  signature: included");
    } else {
        println!("  signature: (none — sign first with `insula sign`)");
    }
    Ok(())
}

// -----------------------------------------------------
// Tiny self-contained TempDir.
//
// We avoid promoting the `tempfile` crate from dev-dep
// to runtime-dep just to extract a `.insula` archive
// during install. The Drop best-effort-removes the
// directory; failures there are not fatal (cleanup is
// a hygiene concern, not a correctness one).
// -----------------------------------------------------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> std::io::Result<Self> {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        for _ in 0..16 {
            let suffix: u64 = rng.next_u64();
            let candidate = std::env::temp_dir()
                .join(format!("{}-{:016x}", prefix, suffix));
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Ok(TempDir { path: candidate }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not find a free temp directory",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
