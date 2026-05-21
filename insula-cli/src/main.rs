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

use insula_bundle::InsulaBundle;
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
  uninstall <app-id>      Remove an installed app + its container.
  help                    Show this help.

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
    let bundle_dir = args.first().ok_or_else(|| {
        "install: missing <bundle-dir> argument".to_string()
    })?;

    let bundle = InsulaBundle::read(bundle_dir)
        .map_err(|e| format!("reading bundle at {}: {}", bundle_dir, e))?;

    std::fs::create_dir_all(install_root)
        .map_err(|e| format!("create install root {}: {}", install_root.display(), e))?;

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

    // If sockets for system services are configured,
    // route them through to the child:
    //   - INSULA_LOGD_SOCKET        -> log forwarding
    //   - INSULA_VESTIBULUMD_SOCKET -> keychain
    let log_socket = std::env::var_os("INSULA_LOGD_SOCKET")
        .map(PathBuf::from);
    let vestibulum_socket = std::env::var_os("INSULA_VESTIBULUMD_SOCKET")
        .map(PathBuf::from);

    // Inherit stdio for `insula launch`; the user wants
    // to see the app's output.
    let mut child = host::launch_installed_full(
        &installed,
        &app_args_raw,
        false,
        log_socket.as_deref(),
        vestibulum_socket.as_deref(),
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
