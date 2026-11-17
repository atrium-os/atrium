//! `cargo atrium <command>`; see `cargo atrium help`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use cargo_atrium::check::{self, Level};

const HELP: &str = "\
cargo atrium: build Atrium apps

Commands:
  check [--manifest <atrium.toml>] [--json]
      Lint the manifest (what the validator says, restricted capabilities,
      the consent text the person will see) and the crate graph (parsers
      and network clients linked into the core, which holds the grants).
      Findings are warnings; the exit status is 1 only for manifest errors.

Run it in the project folder (the one with Cargo.toml and atrium.toml).";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // As a cargo subcommand: `cargo atrium check` runs `cargo-atrium atrium check`.
    if args.first().map(String::as_str) == Some("atrium") {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        Some("check") => cmd_check(&args[1..]),
        None | Some("help") | Some("-h") | Some("--help") => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("cargo atrium: unknown command {other:?}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

fn find_manifest(start: &Path) -> Option<PathBuf> {
    let mut d = Some(start);
    while let Some(x) = d {
        if x.join("atrium.toml").is_file() {
            return Some(x.join("atrium.toml"));
        }
        d = x.parent();
    }
    None
}

fn cmd_check(args: &[String]) -> ExitCode {
    let mut manifest: Option<PathBuf> = None;
    let mut json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--manifest" => manifest = it.next().map(PathBuf::from),
            "--json" => json = true,
            o => {
                eprintln!("cargo atrium check: unknown argument {o:?}");
                return ExitCode::from(2);
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(mpath) = manifest.or_else(|| find_manifest(&cwd)) else {
        eprintln!("cargo atrium check: no atrium.toml here or above");
        return ExitCode::from(2);
    };
    let text = match std::fs::read_to_string(&mpath) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cargo atrium check: {}: {e}", mpath.display());
            return ExitCode::from(2);
        }
    };
    let (mut findings, consent, reach) = check::manifest(&text);
    if let Some(reach) = &reach {
        let dir = mpath.parent().unwrap_or(Path::new("."));
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        match Command::new(cargo).args(["metadata", "--format-version", "1"]).current_dir(dir).output() {
            Ok(o) if o.status.success() => match serde_json::from_slice(&o.stdout).map_err(|e| e.to_string()).and_then(|v| check::binaries(&v)) {
                Ok(bins) => findings.extend(check::graph(reach, &bins)),
                Err(e) => eprintln!("cargo atrium check: crate graph: {e}"),
            },
            Ok(o) => eprintln!("cargo atrium check: cargo metadata failed:\n{}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => eprintln!("cargo atrium check: cargo: {e}"),
        }
    }
    findings.sort_by_key(|f| f.level);
    let errors = findings.iter().any(|f| f.level == Level::Error);
    if json {
        let v = serde_json::json!({
            "manifest": mpath.display().to_string(),
            "consent": consent,
            "findings": findings.iter().map(|f| f.to_json()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        if !consent.is_empty() {
            println!("At install, the person is asked to allow:");
            for c in &consent {
                println!("  - {c}");
            }
            println!();
        }
        for f in &findings {
            let l = match f.level { Level::Error => "error", Level::Warning => "warning", Level::Note => "note" };
            println!("{l}[{}]: {}", f.code, f.message);
            if let Some(fix) = &f.fix {
                println!("  = help: {fix}");
            }
        }
        if findings.is_empty() {
            println!("No findings.");
        }
    }
    if errors { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
