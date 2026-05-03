//! portcullis — CLI front-end.
//!
//! Phase 1 supports one subcommand: `validate <atrium.toml>`.
//! Future phases add `launch`, `pkg refresh`, etc.

use std::fs;
use std::process::ExitCode;

fn usage() -> ! {
    eprintln!("\
usage:
    portcullis validate <atrium.toml>

    Parses and validates an atrium.toml manifest. Prints errors and
    warnings; exits 0 if no errors, 1 otherwise.
");
    std::process::exit(2);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    match args[1].as_str() {
        "validate" => {
            if args.len() != 3 { usage(); }
            cmd_validate(&args[2])
        }
        "--help" | "-h" => { usage() }
        other => {
            eprintln!("portcullis: unknown subcommand {other:?}");
            usage();
        }
    }
}

fn cmd_validate(path: &str) -> ExitCode {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("portcullis validate: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    match portcullis_toml::Manifest::from_str(&text) {
        Err(e) => {
            eprintln!("portcullis validate: {path}: parse error:");
            eprintln!("    {e}");
            ExitCode::from(1)
        }
        Ok(m) => {
            let report = portcullis_toml::validate(&m);
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            for e in &report.errors {
                eprintln!("error:   {e}");
            }
            if report.is_ok() {
                println!("{path}: OK ({} warning{})",
                    report.warnings.len(),
                    if report.warnings.len() == 1 { "" } else { "s" });
                ExitCode::SUCCESS
            } else {
                eprintln!("{path}: FAILED ({} error{}, {} warning{})",
                    report.errors.len(),
                    if report.errors.len() == 1 { "" } else { "s" },
                    report.warnings.len(),
                    if report.warnings.len() == 1 { "" } else { "s" });
                ExitCode::from(1)
            }
        }
    }
}
