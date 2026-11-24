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
  new <name> [--template <t>] [--id <app id>] [--sdk-path <atrium checkout>]
      A project in ./<name> from an SDK template; --list [--json] shows them.
  cross <amd64|aarch64> [--sysroot <dir>] [cargo build args]
      Build for Atrium on that arch from Linux, macOS or the other arch:
      clang and ld.lld with the SDK sysroot ($ATRIUM_SYSROOT/<arch> or
      ~/.atrium/sysroot/<arch>), then cargo build --target <triple>.
  run [--release] [cargo build args]
      In a workspace: build, put the core and each worker where
      atrium.toml's entries say, and ask (in the Terminal, with your press) to Run the project as an
      app, with the capabilities its manifest asks for.
  worker add <name> [--input fd|bytes|message] [--from <path::fn>]
             [--lifetime call|session] [--dry-run] [--json]
      Move a part of the app into a [[workers]] entry: the manifest entry,
      a [[bin]], src/<name>_worker.rs (the messages, the handler and the
      core's call) and src/bin/<name>.rs. --from runs that library
      function in the worker and lists the core's calls to it to replace
      with <name>_worker::call. --json: what changed, for Artifex.
  bundle [--arch amd64|aarch64] [--sign <key.pem>] [--out <dir>]
         [--sysroot <dir>] [--no-strip] [--json] [-- cargo build args]
      Build release and write an installable bundle: atrium.toml, the core
      and each worker at their entries, the icon, and every library they
      need with the loader (read from the programs, never run). Then
      opifex pack (with --sign, the publisher's signature) and opifex
      archive to <out>.insula, which installs through the install sheet.
      Programs are stripped; their debug info goes to <out>.debug/.
      Without Cargo.toml (a C project) it bundles what make built.
  fuzz --list [--json]
  fuzz <target> [--json] [--max-time <secs>]
      Fuzz a worker or an input surface of the core with libFuzzer
      (targets: each [[workers]] entry, as worker:<name>, through its
      handler mylib::workers::<name>; and package.metadata.atrium.fuzz
      tables named worker:x or core:x in Cargo.toml, with function =
      mylib::f). Stops at the first crash; the
      input is saved in fuzz/crashes/<target>/. --json: one event a line.
  registry verify <folder or https URL> [--keys <dir or .pem>]
      Check the SDK registry's index against the platform keys
      (default /etc/atrium/platform-keys): index.sig over index.files,
      and every listed file. Exit status 1 on any mismatch.
  registry build --out <dir> --url <https://host/sdk> [--sign <key.pem>] <seed>...
      For the SDK's maintainers: pack every path crate the seed projects
      reach into a static sparse registry and sign its index.

Run it in the project folder (the one with Cargo.toml and atrium.toml).";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // As a cargo subcommand: `cargo atrium check` runs `cargo-atrium atrium check`.
    if args.first().map(String::as_str) == Some("atrium") {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        Some("check") => cmd_check(&args[1..]),
        Some("new") => cmd_new(&args[1..]),
        Some("cross") => cmd_cross(&args[1..]),
        Some("registry") => cmd_registry(&args[1..]),
        Some("fuzz") => cmd_fuzz(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some("worker") => cmd_worker(&args[1..]),
        Some("bundle") => cmd_bundle(&args[1..]),
        Some("icon") => cmd_icon(&args[1..]),
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

fn cmd_icon(args: &[String]) -> ExitCode {
    use cargo_atrium::icon;
    let sub = args.first().map(String::as_str);
    let (mut file, mut name, mut json, mut dry) = (None, None, false, false);
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--dry-run" => dry = true,
            "--name" => name = it.next().cloned(),
            o if o.starts_with("--") => {
                eprintln!("cargo atrium icon: unknown flag {o}");
                return ExitCode::from(2);
            }
            o => file = Some(PathBuf::from(o)),
        }
    }
    let fail = |e: String| {
        if json {
            println!("{}", serde_json::json!({"ok": false, "error": e}));
        } else {
            eprintln!("cargo atrium icon: {e}");
        }
        ExitCode::FAILURE
    };
    let Some(file) = file else { return fail("an SVG file, e.g. cargo atrium icon check assets/icons/app.svg".into()) };
    match sub {
        Some("check") => {
            let r = match icon::read_source(&file) {
                Ok(b) => atrium_icon::check(&b),
                Err(e) => return fail(e),
            };
            if json {
                println!("{}", icon::report_json(&r));
            } else {
                println!("{}: {} shapes, {} points, {} bytes", file.display(), r.shapes, r.points, r.bytes);
                match &r.problem {
                    None => println!("  the dock draws it"),
                    Some(why) => println!("  the dock won't draw it: {why}"),
                }
            }
            if r.ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some("import") => {
            let proj = PathBuf::from(".");
            let p = match icon::plan(&proj, &file, name.as_deref()) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            let ok = p.import.report.ok();
            if !dry && ok {
                if let Err(e) = icon::apply(&proj, &p) {
                    return fail(e);
                }
            }
            if json {
                let v = serde_json::json!({
                    "ok": ok, "dry_run": dry, "path": p.path, "replaces": p.replaces,
                    "report": icon::report_json(&p.import.report),
                    "converted": p.import.converted, "dropped": p.import.dropped,
                    "svg": if dry { Some(&p.import.svg) } else { None },
                });
                println!("{v}");
            } else {
                let verb = match (ok, dry) { (false, _) => "can't import", (true, true) => "would write", _ => "wrote" };
                println!("cargo atrium: {verb} {}{}", p.path, if p.replaces { " (replacing it)" } else { "" });
                let r = &p.import.report;
                println!("  {} shapes, {} points, {} bytes", r.shapes, r.points, r.bytes);
                for c in &p.import.converted { println!("  converted: {c}") }
                for d in &p.import.dropped { println!("  dropped:   {d}") }
                if let Some(why) = &r.problem { println!("  the dock won't draw it: {why}") }
            }
            if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        _ => {
            eprintln!("cargo atrium icon: check <file.svg> [--json] | import <file.svg> [--name <n>] [--dry-run] [--json]");
            ExitCode::from(2)
        }
    }
}

fn cmd_worker(args: &[String]) -> ExitCode {
    use cargo_atrium::worker;
    if args.first().map(String::as_str) != Some("add") {
        eprintln!("cargo atrium worker: add <name> [--input fd|bytes|message] [--from <path::fn>] [--lifetime call|session] [--dry-run] [--json]");
        return ExitCode::from(2);
    }
    let (mut name, mut input, mut from, mut lifetime) = (None, "fd".to_string(), None, "call".to_string());
    let (mut json, mut dry) = (false, false);
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--dry-run" => dry = true,
            "--input" => input = it.next().cloned().unwrap_or_default(),
            "--from" => from = it.next().cloned(),
            "--lifetime" => lifetime = it.next().cloned().unwrap_or_default(),
            o if o.starts_with("--") => {
                eprintln!("cargo atrium worker add: unknown flag {o}");
                return ExitCode::from(2);
            }
            o => name = Some(o.to_string()),
        }
    }
    let fail = |e: String| {
        if json {
            println!("{}", serde_json::json!({"ok": false, "error": e}));
        } else {
            eprintln!("cargo atrium worker add: {e}");
        }
        ExitCode::FAILURE
    };
    let Some(name) = name else { return fail("a name, e.g. cargo atrium worker add decode".into()) };
    let Some(input) = worker::Input::parse(&input) else { return fail(format!("--input {input:?}: fd, bytes or message")) };
    let o = worker::AddOpts { name: &name, input, from: from.as_deref(), lifetime: &lifetime };
    let proj = PathBuf::from(".");
    let plan = match worker::plan(&proj, &o) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    if !dry {
        if let Err(e) = worker::apply(&proj, &plan) {
            return fail(e);
        }
    }
    if json {
        let v = serde_json::json!({
            "ok": true,
            "dry_run": dry,
            "worker": name,
            "input": input.as_str(),
            "changed": plan.writes.iter().map(|w| serde_json::json!({"path": w.path, "action": if w.created { "created" } else { "edited" }})).collect::<Vec<_>>(),
            "core_call": plan.core_call,
            "fuzz_target": plan.fuzz_target,
            "calls": plan.calls.iter().map(|c| serde_json::json!({"path": c.path, "line": c.line, "text": c.text})).collect::<Vec<_>>(),
            "notes": plan.notes,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        println!("cargo atrium: {} worker {name:?} (input {}):", if dry { "would add" } else { "added" }, input.as_str());
        for w in &plan.writes {
            println!("  {} {}", if w.created { "created" } else { "edited " }, w.path);
        }
        println!("  the core calls it with {}(...); fuzz it with cargo atrium fuzz {}", plan.core_call, plan.fuzz_target);
        for c in &plan.calls {
            println!("  call to replace: {}:{}: {}", c.path, c.line, c.text);
        }
        for n in &plan.notes {
            println!("  note: {n}");
        }
    }
    ExitCode::SUCCESS
}

/// The project's `target_directory` (of `manifest`, or the one here).
fn target_dir(manifest: Option<&Path>) -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut c = Command::new(&cargo);
    c.args(["metadata", "--no-deps", "--format-version", "1"]);
    if let Some(m) = manifest {
        c.arg("--manifest-path").arg(m);
    }
    c.output().ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v["target_directory"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| manifest.and_then(Path::parent).unwrap_or(Path::new(".")).join("target"))
}

/// The core and the workers, and where each is built (programs.rs).
fn project_programs(proj: &Path, atrium_toml: &str) -> Result<Vec<cargo_atrium::programs::Program>, String> {
    let md = if proj.join("Cargo.toml").is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let o = Command::new(&cargo).args(["metadata", "--format-version", "1"]).current_dir(proj).output()
            .map_err(|e| format!("{cargo}: {e}"))?;
        if !o.status.success() {
            return Err(format!("cargo metadata failed:\n{}", String::from_utf8_lossy(&o.stderr)));
        }
        Some(serde_json::from_slice::<serde_json::Value>(&o.stdout).map_err(|e| e.to_string())?)
    } else {
        None
    };
    cargo_atrium::programs::locate(proj, atrium_toml, md.as_ref())
}

/// Build every program, natively or with `cargo atrium cross` (arch and
/// sysroot): (entry, the built binary). `args` go to each cargo build.
fn build_programs(progs: &[cargo_atrium::programs::Program], release: bool, cross: Option<(&cargo_atrium::cross::Target, &Path)>,
                  args: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    use cargo_atrium::programs::{self, Source};
    if let Some(p) = progs.iter().find(|p| p.source.is_none()) {
        return Err(format!("no binary {:?} for {} (entry {}): name a [[bin]] after it, or say where it is built under [package.metadata.atrium.workers]",
            p.bin, p.worker.as_deref().map(|w| format!("the {w} worker")).unwrap_or_else(|| "the core".into()), p.entry));
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    for b in programs::builds(progs) {
        let mut c = match cross {
            Some((t, root)) => {
                let mut c = Command::new(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cargo-atrium")));
                c.args(["atrium", "cross", t.arch, "--sysroot"]).arg(root);
                c
            }
            None => {
                let mut c = Command::new(&cargo);
                c.arg("build");
                c
            }
        };
        if let Some(m) = &b.manifest {
            c.arg("--manifest-path").arg(m);
        }
        c.args(&b.args).args(args);
        match c.status() {
            Ok(s) if s.success() => {}
            _ => return Err(format!("the build failed ({}{})", b.manifest.as_ref().map(|m| format!("{}: ", m.display())).unwrap_or_default(), b.args.join(" "))),
        }
    }
    let profile = if release { "release" } else { "debug" };
    let here = target_dir(None);
    Ok(progs.iter().map(|p| {
        let base = match &p.source {
            Some(Source::Elsewhere(m)) => target_dir(Some(m)),
            _ => here.clone(),
        };
        let dir = match cross { Some((t, _)) => base.join(t.triple), None => base };
        (p.entry.clone(), dir.join(profile).join(&p.bin))
    }).collect())
}

fn cmd_bundle(args: &[String]) -> ExitCode {
    use cargo_atrium::{bundle, cross};
    let (mut arch, mut sign, mut out, mut sysroot, mut json, mut rest) = (None, None, None, None, false, Vec::new());
    let mut strip = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--arch" => arch = it.next().cloned(),
            "--sign" => sign = it.next().map(PathBuf::from),
            "--out" => out = it.next().map(PathBuf::from),
            "--sysroot" => sysroot = it.next().map(PathBuf::from),
            "--json" => json = true,
            "--no-strip" => strip = false,
            "--" => rest.extend(it.by_ref().cloned()),
            o => {
                eprintln!("cargo atrium bundle: unknown argument {o} (cargo's go after --)");
                return ExitCode::from(2);
            }
        }
    }
    let fail = |e: String| {
        if json {
            println!("{}", serde_json::json!({"ok": false, "error": e}));
        } else {
            eprintln!("cargo atrium bundle: {e}");
        }
        ExitCode::FAILURE
    };
    let host = if cfg!(target_arch = "aarch64") { "aarch64" } else { "amd64" };
    let Some(t) = cross::target(arch.as_deref().unwrap_or(host)) else { return fail("--arch: amd64 or aarch64".into()) };
    let native = cfg!(target_os = "freebsd") && t.arch == host;
    let manifest = match std::fs::read_to_string("atrium.toml") {
        Ok(m) => m,
        Err(e) => return fail(format!("atrium.toml: {e} (run it in the project folder)")),
    };
    let mv: toml::Value = match manifest.parse() {
        Ok(v) => v,
        Err(e) => return fail(format!("atrium.toml: {e}")),
    };
    let app = |k: &str| mv.get("app").and_then(|a| a.get(k)).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (id, version) = (app("id"), app("version"));
    if id.is_empty() || version.is_empty() {
        return fail("atrium.toml: [app] needs an id and a version".into());
    }
    if id.starts_with("dev.") {
        return fail(format!("{id}: a dev. id is Run from its workspace, not bundled; give [app] an id of your own (reverse DNS of a domain you control)"));
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    // Without Cargo.toml (a C project, after `make`): the programs are
    // already at their entries.
    let prebuilt = !Path::new("Cargo.toml").is_file();
    let tdir = if prebuilt { PathBuf::from("bundle") } else { target_dir(None) };
    let progs = match project_programs(Path::new("."), &manifest) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };

    // 1. Build.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let root = if native {
        PathBuf::from("/")
    } else if prebuilt && cfg!(target_os = "freebsd") {
        PathBuf::from("/")
    } else {
        match cross::sysroot(t, sysroot.as_deref(), std::env::var("ATRIUM_SYSROOT").ok().as_deref(), home.as_deref()) {
            Ok(s) => std::fs::canonicalize(&s).unwrap_or(s),
            Err(e) => return fail(e),
        }
    };
    let built: Vec<(String, PathBuf)> = if prebuilt {
        progs.iter().map(|p| (p.entry.clone(), PathBuf::from(&p.entry))).collect()
    } else {
        let mut rest = rest.clone();
        rest.insert(0, "--release".into());
        match build_programs(&progs, true, (!native).then_some((t, root.as_path())), &rest) {
            Ok(b) => b,
            Err(e) => return fail(e),
        }
    };
    let _ = &cargo;

    // 2. The tree.
    let name = format!("{id}-{version}-{}", t.arch);
    let tree = out.unwrap_or_else(|| if prebuilt { tdir.join(&name) } else { tdir.join("atrium-bundle").join(&name) });
    if tree.exists() {
        if !tree.join("atrium.toml").is_file() {
            return fail(format!("{} exists and isn't a bundle; give --out another folder", tree.display()));
        }
        if let Err(e) = std::fs::remove_dir_all(&tree) {
            return fail(format!("{}: {e}", tree.display()));
        }
    }
    let mut placed: Vec<String> = Vec::new();
    let mut place = |from: &Path, to: &str| -> Result<(), String> {
        bundle::place(from, &tree, to)?;
        placed.push(to.to_string());
        Ok(())
    };
    let mut programs = Vec::new();
    let mut steps = vec![(PathBuf::from("atrium.toml"), "atrium.toml".to_string())];
    for (entry, from) in &built {
        if !from.is_file() {
            return fail(format!("{} isn't there{}", from.display(), if prebuilt { " (build it first, e.g. make)" } else { "" }));
        }
        steps.push((from.clone(), entry.clone()));
        programs.push(from.clone());
    }
    for f in bundle::named_files(&manifest) {
        steps.push((PathBuf::from(&f), f.clone()));
    }
    // The pictures and icons atrium-assets opens, at the same paths.
    match cargo_atrium::icon::asset_files(Path::new(".")) {
        Ok(files) => steps.extend(files.into_iter().map(|f| (PathBuf::from(&f), f))),
        Err(e) => return fail(e),
    }
    for (from, to) in &steps {
        if let Err(e) = place(from, to) {
            return fail(e);
        }
    }
    // Strip the programs; their debug info goes beside the tree, linked
    // by .gnu_debuglink, for the debugger.
    let mut debug_dir = None;
    if strip && !prebuilt {
        let path = std::env::var("PATH").unwrap_or_default();
        match cross::find_tool(&["llvm-objcopy", "llvm-objcopy-21", "llvm-objcopy-20", "llvm-objcopy-19", "llvm-objcopy-18", "objcopy"], &path) {
            Some(oc) => {
                let dd = PathBuf::from(format!("{}.debug", tree.display()));
                let _ = std::fs::remove_dir_all(&dd);
                for (entry, _) in &built {
                    let prog = tree.join(entry);
                    let dbg = dd.join(format!("{entry}.debug"));
                    let ok = dbg.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).is_ok()
                        && Command::new(&oc).arg("--only-keep-debug").arg(&prog).arg(&dbg).status().is_ok_and(|s| s.success())
                        && Command::new(&oc).arg("--strip-all").arg(format!("--add-gnu-debuglink={}", dbg.display())).arg(&prog).status().is_ok_and(|s| s.success());
                    if !ok {
                        return fail(format!("{}: strip failed (--no-strip skips it)", oc.display()));
                    }
                }
                debug_dir = Some(dd);
            }
            None => eprintln!("cargo atrium bundle: no objcopy on PATH, so the programs keep their debug info"),
        }
    }
    // 3. The closure.
    let extra = match bundle::extra_libs(&manifest) {
        Ok(e) => e,
        Err(e) => return fail(e),
    };
    let libs = match bundle::closure(&programs, &root, &extra) {
        Ok(c) => c,
        Err(e) => return fail(if native { e } else { format!("{e}; from outside Atrium an app can only need base libraries (bundle in a workspace for the others)") }),
    };
    for s in &libs {
        if let Err(e) = place(&s.from, &s.to) {
            return fail(e);
        }
    }
    // 4. Pack, sign, archive.
    let path = std::env::var("PATH").unwrap_or_default();
    let opifex = std::env::var_os("OPIFEX").map(PathBuf::from)
        .or_else(|| cross::find_tool(&["opifex"], &path))
        .or_else(|| ["/usr/local/bin/opifex", "/usr/bin/opifex", "/usr/sbin/opifex"].iter().map(PathBuf::from).find(|p| p.is_file()));
    let insula = tree.with_extension("insula");
    let insula = if tree.extension().is_some() { PathBuf::from(format!("{}.insula", tree.display())) } else { insula };
    let mut packed = false;
    match &opifex {
        Some(op) => {
            // With --json, stdout is the JSON alone: opifex's lines go to stderr.
            let quiet = |c: &mut Command| {
                if json {
                    use std::os::fd::AsFd;
                    if let Ok(e) = std::io::stderr().as_fd().try_clone_to_owned() {
                        c.stdout(std::process::Stdio::from(e));
                    }
                }
            };
            let mut c = Command::new(op);
            quiet(&mut c);
            c.arg("pack").arg(&tree);
            if let Some(k) = &sign {
                c.arg("--sign").arg(k);
            }
            match c.status() {
                Ok(s) if s.success() => {}
                _ => return fail("opifex pack failed".into()),
            }
            let mut c = Command::new(op);
            quiet(&mut c);
            match c.arg("archive").arg(&tree).arg(&insula).status() {
                Ok(s) if s.success() => {}
                _ => return fail("opifex archive failed".into()),
            }
            packed = true;
        }
        None if sign.is_some() => return fail("--sign needs opifex (on Atrium it's in the base; elsewhere set OPIFEX)".into()),
        None => {}
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "ok": true, "id": id, "version": version, "arch": t.arch,
            "tree": tree.display().to_string(),
            "insula": packed.then(|| insula.display().to_string()),
            "signed": packed && sign.is_some(),
            "debug": debug_dir.as_ref().map(|d| d.display().to_string()),
            "files": placed,
        })).unwrap_or_default());
    } else {
        println!("cargo atrium: bundle {} ({} programs, {} libraries and the loader)", tree.display(), progs.len(), libs.len().saturating_sub(1));
        if packed {
            println!("  {}{}", insula.display(), if sign.is_some() { ", signed" } else { ", NOT signed: it installs only with --allow-unsigned" });
        } else {
            println!("  not packed: no opifex here (set OPIFEX); on Atrium: opifex pack {} --sign <key>", tree.display());
        }
    }
    ExitCode::SUCCESS
}

fn cmd_new(args: &[String]) -> ExitCode {
    use cargo_atrium::new;
    let (mut name, mut template, mut id, mut sdk, mut registry) = (None, "app".to_string(), None, None, new::REGISTRY.to_string());
    let (mut list, mut json) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => list = true,
            "--json" => json = true,
            "--template" => template = it.next().cloned().unwrap_or_default(),
            "--id" => id = it.next().cloned(),
            "--sdk-path" => sdk = it.next().map(PathBuf::from),
            "--registry" => registry = it.next().cloned().unwrap_or(registry),
            o if o.starts_with("--") => {
                eprintln!("cargo atrium new: unknown flag {o}");
                return ExitCode::from(2);
            }
            o => name = Some(o.to_string()),
        }
    }
    if list {
        if json {
            let v: Vec<_> = new::TEMPLATES.iter().map(|(t, what, _)| serde_json::json!({"name": t, "about": what})).collect();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        } else {
            for (t, what, _) in new::TEMPLATES {
                println!("  {t:<10} {what}");
            }
        }
        return ExitCode::SUCCESS;
    }
    let Some(dir) = name else {
        eprintln!("cargo atrium new: a name, e.g. cargo atrium new hello");
        return ExitCode::from(2);
    };
    // Like cargo new: the argument is a path, the name its last component.
    let name = Path::new(&dir).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let sdk = sdk.map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    let o = new::Opts { name: &name, template: &template, id: id.as_deref(), sdk_path: sdk.as_deref(), registry: &registry };
    match new::create(Path::new(&dir), &o) {
        Ok(files) => {
            println!("cargo atrium: made {dir}/ from the {template} template: {}", files.join(", "));
            let build = if template == "c-app" { "make" } else { "cargo build" };
            println!("  next: cd {dir} && cargo atrium check && {build}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cargo atrium new: {e}");
            ExitCode::FAILURE
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

/// Each member's binaries and what they link as built: the member's own
/// `cargo tree -e normal` for Atrium (no features turned on only by
/// dev-dependencies), falling back to the metadata's merged resolve.
fn built_binaries(dir: &Path, md: &serde_json::Value) -> Result<std::collections::BTreeMap<String, std::collections::BTreeMap<String, check::Linked>>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let members: Vec<String> = md.get("workspace_members").and_then(|m| m.as_array()).into_iter().flatten()
        .filter_map(|m| m.as_str()).map(str::to_string).collect();
    let mut out = std::collections::BTreeMap::new();
    for id in &members {
        let Some(p) = md["packages"].as_array().and_then(|ps| ps.iter().find(|p| p["id"] == id.as_str())) else { continue };
        let spec = format!("{}@{}", p["name"].as_str().unwrap_or(""), p["version"].as_str().unwrap_or(""));
        let tree = Command::new(&cargo)
            .args(["tree", "-e", "normal", "--target", "x86_64-unknown-freebsd", "--prefix", "depth", "-f", "{p}", "-p", &spec])
            .current_dir(dir).output().ok().filter(|o| o.status.success());
        let mut one = md.clone();
        one["workspace_members"] = serde_json::json!([id]);
        if let Some(t) = tree {
            one["resolve"]["nodes"] = serde_json::Value::Array(check::nodes_from_tree(&String::from_utf8_lossy(&t.stdout), md));
        }
        out.extend(check::binaries(&one)?);
    }
    Ok(out)
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
    let dir = mpath.parent().unwrap_or(Path::new("."));
    // A C project has no crate graph to read.
    if let Some(reach) = reach.as_ref().filter(|_| dir.join("Cargo.toml").is_file()) {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        match Command::new(cargo).args(["metadata", "--format-version", "1"]).current_dir(dir).output() {
            Ok(o) if o.status.success() => match serde_json::from_slice::<serde_json::Value>(&o.stdout).map_err(|e| e.to_string())
                .and_then(|v| Ok((built_binaries(dir, &v)?, cargo_atrium::programs::locate(dir, &text, Some(&v))?))) {
                Ok((bins, progs)) => {
                    // Workers built in another crate (programs.rs): their
                    // links are their own business, as for any worker.
                    let elsewhere: std::collections::BTreeSet<String> = progs.into_iter()
                        .filter(|p| matches!(p.source, Some(cargo_atrium::programs::Source::Dependency(_) | cargo_atrium::programs::Source::Elsewhere(_))))
                        .map(|p| p.bin).collect();
                    findings.extend(check::graph(reach, &bins, &elsewhere))
                }
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

fn cmd_cross(args: &[String]) -> ExitCode {
    use cargo_atrium::cross;
    let fail = |m: String| {
        eprintln!("cargo atrium cross: {m}");
        ExitCode::FAILURE
    };
    let (mut arch, mut given, mut rest) = (None, None, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sysroot" => given = it.next().map(PathBuf::from),
            o if arch.is_none() && !o.starts_with('-') => arch = Some(o.to_string()),
            o => rest.push(o.to_string()),
        }
    }
    let Some(t) = arch.as_deref().and_then(cross::target) else {
        return fail("which arch? amd64 or aarch64".into());
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let sysroot = match cross::sysroot(t, given.as_deref(), std::env::var("ATRIUM_SYSROOT").ok().as_deref(), home.as_deref()) {
        Ok(s) => std::fs::canonicalize(&s).unwrap_or(s),
        Err(e) => return fail(e),
    };
    let path = std::env::var("PATH").unwrap_or_default();
    let clang = match std::env::var_os("ATRIUM_CLANG").map(PathBuf::from) {
        Some(c) => c,
        None => match cross::find_tool(&["clang", "clang-21", "clang-20", "clang-19", "clang-18"], &path) {
            Some(c) => c,
            None => return fail("no clang on PATH (set ATRIUM_CLANG)".into()),
        },
    };
    if cross::find_tool(&["ld.lld"], &path).is_none() && !clang.with_file_name("ld.lld").is_file() {
        return fail("no ld.lld on PATH or next to clang (install lld)".into());
    }
    let ar = cross::find_tool(&["llvm-ar", "llvm-ar-21", "llvm-ar-20", "llvm-ar-19", "llvm-ar-18"], &path);
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let build_std = rest.iter().any(|a| a.starts_with("-Zbuild-std"));
    match Command::new(&rustc).args(["--print", "target-libdir", "--target", t.triple]).output() {
        _ if build_std => {}
        Ok(o) if o.status.success() && cross::has_std(Path::new(String::from_utf8_lossy(&o.stdout).trim())) => {}
        // No prebuilt std (aarch64 is tier 3): a nightly with rust-src
        // builds it, so pass the flag rather than asking for it.
        _ if cross::can_build_std(&rustc) => rest.push("-Zbuild-std=std,panic_abort".into()),
        _ => return fail(format!("this rustc has no std for {}: {}", t.triple, cross::std_hint(t))),
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let tdir = match Command::new(&cargo).args(["metadata", "--no-deps", "--format-version", "1"]).output() {
        Ok(o) if o.status.success() => serde_json::from_slice::<serde_json::Value>(&o.stdout)
            .ok()
            .and_then(|v| v["target_directory"].as_str().map(PathBuf::from)),
        _ => None,
    };
    let Some(tdir) = tdir else { return fail("cargo metadata failed; run it in the project folder".into()) };
    let wdir = tdir.join("atrium-cross").join(t.arch);
    let mut wrappers = Vec::new();
    for kind in ["cc", "c++"] {
        let p = wdir.join(format!("{}-freebsd-{kind}", t.cpu));
        let r = std::fs::create_dir_all(&wdir).and_then(|_| std::fs::write(&p, cross::wrapper(t, &clang, &sysroot, kind)));
        #[cfg(unix)]
        let r = r.and_then(|_| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
        });
        if let Err(e) = r {
            return fail(format!("{}: {e}", p.display()));
        }
        wrappers.push(p);
    }
    eprintln!("cargo atrium cross: {} with {} and {}", t.triple, clang.display(), sysroot.display());
    let st = Command::new(&cargo)
        .args(["build", "--target", t.triple])
        .args(&rest)
        .envs(cross::env(t, &wrappers[0], &wrappers[1], ar.as_deref(), &sysroot))
        .envs(cross::rustflags_env(std::env::var_os("RUSTFLAGS").is_some() || std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some()))
        .status();
    match st {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(1, 255) as u8),
        Err(e) => fail(format!("{cargo}: {e}")),
    }
}

/// The platform keys: one PEM file, or every `.pem` in a folder.
fn read_keys(dir: &Path) -> Result<Vec<String>, String> {
    if dir.is_file() {
        return std::fs::read_to_string(dir).map(|k| vec![k]).map_err(|e| format!("{}: {e}", dir.display()));
    }
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        if e.path().extension().map(|x| x == "pem").unwrap_or(false) {
            v.push(std::fs::read_to_string(e.path()).map_err(|e| e.to_string())?);
        }
    }
    if v.is_empty() {
        return Err(format!("{}: no .pem keys", dir.display()));
    }
    Ok(v)
}

fn cmd_registry(args: &[String]) -> ExitCode {
    use cargo_atrium::registry;
    let fail = |m: String| {
        eprintln!("cargo atrium registry: {m}");
        ExitCode::FAILURE
    };
    let (mut out, mut url, mut key, mut keys, mut pos) = (None, None, None, PathBuf::from("/etc/atrium/platform-keys"), Vec::new());
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = it.next().map(PathBuf::from),
            "--url" => url = it.next().cloned(),
            "--sign" => key = it.next().map(PathBuf::from),
            "--keys" => keys = it.next().map(PathBuf::from).unwrap_or(keys),
            o if o.starts_with("--") => return fail(format!("unknown flag {o}")),
            o => pos.push(o.to_string()),
        }
    }
    match args.first().map(String::as_str) {
        Some("verify") => {
            let [src] = &pos[..] else { return fail("verify <folder or URL>".into()) };
            let keys = match read_keys(&keys) {
                Ok(k) => k,
                Err(e) => return fail(e),
            };
            let base = src.trim_end_matches('/').to_string();
            let remote = base.starts_with("https://");
            let fetch = |p: &str| -> Result<Vec<u8>, String> {
                if remote {
                    let u = format!("{base}/index/{p}");
                    let o = Command::new("curl").args(["-fsS", "--proto", "=https", &u]).output().map_err(|e| format!("curl: {e}"))?;
                    if o.status.success() { Ok(o.stdout) } else { Err(format!("{u}: {}", String::from_utf8_lossy(&o.stderr).trim())) }
                } else {
                    std::fs::read(Path::new(&base).join("index").join(p)).map_err(|e| format!("{p}: {e}"))
                }
            };
            match registry::verify(&fetch, &keys) {
                Ok(n) => {
                    println!("cargo atrium registry: {base}: index signed by a platform key, {n} files match");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        Some("build") => {
            let (Some(out), Some(url)) = (out, url) else { return fail("build needs --out and --url".into()) };
            if pos.is_empty() {
                return fail("build needs at least one seed project".into());
            }
            let key_pem = match key.map(std::fs::read_to_string).transpose() {
                Ok(k) => k,
                Err(e) => return fail(format!("--sign: {e}")),
            };
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let mut metas = Vec::new();
            for seed in &pos {
                match Command::new(&cargo).args(["metadata", "--format-version", "1"]).current_dir(seed).output() {
                    Ok(o) if o.status.success() => match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                        Ok(v) => metas.push(v),
                        Err(e) => return fail(format!("{seed}: {e}")),
                    },
                    Ok(o) => return fail(format!("{seed}: cargo metadata: {}", String::from_utf8_lossy(&o.stderr))),
                    Err(e) => return fail(format!("{cargo}: {e}")),
                }
            }
            let mut pkgs: Vec<registry::Pkg> = Vec::new();
            for m in &metas {
                let roots: Vec<String> = m["workspace_members"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| m["packages"].as_array()?.iter().find(|p| p["id"] == *id)?["name"].as_str().map(str::to_string))
                    .collect();
                match registry::sdk_packages(m, &roots) {
                    Ok(ps) => {
                        for p in ps {
                            if !pkgs.iter().any(|q| q.name == p.name) {
                                pkgs.push(p);
                            }
                        }
                    }
                    Err(e) => return fail(e),
                }
            }
            pkgs.sort_by(|a, b| a.name.cmp(&b.name));
            let o = registry::BuildOpts { out: &out, url: &url, sign_key_pem: key_pem.as_deref() };
            match registry::build(&pkgs, &o) {
                Ok(made) => {
                    for (n, v, b) in &made {
                        println!("  {n} {v} ({b} bytes)");
                    }
                    println!("cargo atrium registry: {} crates in {}{}", made.len(), out.display(), if key_pem.is_some() { ", index signed" } else { ", index NOT signed" });
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        _ => fail("build or verify".into()),
    }
}

fn cmd_fuzz(args: &[String]) -> ExitCode {
    use cargo_atrium::fuzz;
    use std::io::{BufRead, BufReader};
    let (mut list, mut jsonl, mut max_time, mut name) = (false, false, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => list = true,
            "--json" => jsonl = true,
            "--max-time" => max_time = it.next().and_then(|v| v.parse::<u64>().ok()),
            o if o.starts_with("--") => {
                eprintln!("cargo atrium fuzz: unknown flag {o}");
                return ExitCode::from(2);
            }
            o => name = Some(o.to_string()),
        }
    }
    let fail = |m: String| {
        if jsonl {
            println!("{}", serde_json::json!({"event": "error", "message": m}));
        } else {
            eprintln!("cargo atrium fuzz: {m}");
        }
        ExitCode::FAILURE
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut d = Some(cwd.as_path());
    let mut manifest = None;
    while let Some(x) = d {
        if x.join("Cargo.toml").is_file() {
            manifest = Some(x.join("Cargo.toml"));
            break;
        }
        d = x.parent();
    }
    let Some(manifest) = manifest else { return fail("no Cargo.toml here or above".into()) };
    let project = manifest.parent().unwrap_or(Path::new(".")).to_path_buf();
    let text = match std::fs::read_to_string(&manifest) {
        Ok(t) => t,
        Err(e) => return fail(format!("{}: {e}", manifest.display())),
    };
    let atrium_toml = std::fs::read_to_string(project.join("atrium.toml")).ok();
    let targets = match fuzz::targets(&text, atrium_toml.as_deref()) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    if list {
        if jsonl {
            let v: Vec<_> = targets.iter().map(|t| t.to_json()).collect();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        } else if targets.is_empty() {
            println!("No fuzz targets: add [[workers]] to atrium.toml, or declare them in Cargo.toml as [package.metadata.atrium.fuzz.\"core:<surface>\"] function = \"mylib::f\"");
        } else {
            for t in &targets {
                println!("  {:<24} {}", t.name, t.about);
            }
        }
        return ExitCode::SUCCESS;
    }
    let Some(name) = name else { return fail("which target? (--list shows them)".into()) };
    let Some(t) = targets.iter().find(|t| t.name == name) else { return fail(format!("no fuzz target {name:?}")) };
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let meta = match Command::new(&cargo).args(["metadata", "--no-deps", "--format-version", "1"]).current_dir(&project).output() {
        Ok(o) if o.status.success() => serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap_or_default(),
        _ => return fail("cargo metadata failed".into()),
    };
    let Some(lib) = fuzz::lib_name(&meta, &manifest) else {
        return fail("the project has no lib crate: put the fuzzed functions in src/lib.rs".into());
    };
    let tdir = meta["target_directory"].as_str().map(PathBuf::from).unwrap_or_else(|| project.join("target"));
    let hdir = tdir.join("atrium-fuzz").join(t.slug());
    let package = text.parse::<toml::Value>().ok()
        .and_then(|v| v.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_else(|| lib.clone());
    let (hc, hm) = match fuzz::harness(t, &lib, &package, &project, fuzz::worker_dependency(&text, &project).as_deref()) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    if let Err(e) = std::fs::create_dir_all(hdir.join("src"))
        .and_then(|_| std::fs::write(hdir.join("Cargo.toml"), hc))
        .and_then(|_| std::fs::write(hdir.join("src/main.rs"), hm))
    {
        return fail(format!("{}: {e}", hdir.display()));
    }
    let htarget = tdir.join("atrium-fuzz").join("target");
    // With --target, RUSTFLAGS reach only the harness and its dependencies,
    // not build scripts and proc macros (which have no fuzzer to link).
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let host = Command::new(&rustc).arg("-vV").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("host: ").map(str::to_string)));
    let Some(host) = host else { return fail(format!("{rustc} -vV: no host")) };
    let mut b = Command::new(&cargo);
    b.args(["build", "--release", "--target", &host, "--manifest-path"]).arg(hdir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &htarget)
        .env("RUSTFLAGS", fuzz::RUSTFLAGS);
    if jsonl {
        b.arg("-q").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        match b.output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr);
                let tail: Vec<&str> = e.lines().rev().take(30).collect();
                return fail(format!("build failed:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n")));
            }
            Err(e) => return fail(format!("{cargo}: {e}")),
        }
    } else {
        match b.status() {
            Ok(s) if s.success() => {}
            _ => return fail("build failed".into()),
        }
    }
    let binary = htarget.join(&host).join("release").join(format!("atrium-fuzz-{}", t.slug()));
    let corpus = Path::new("fuzz/corpus").join(t.slug());
    let crashes = Path::new("fuzz/crashes").join(t.slug());
    for d in [&corpus, &crashes] {
        if let Err(e) = std::fs::create_dir_all(project.join(d)) {
            return fail(format!("{}: {e}", d.display()));
        }
    }
    let start = std::time::Instant::now();
    let mut child = match Command::new(&binary)
        .args(fuzz::fuzzer_args(&corpus, &crashes, max_time))
        .current_dir(&project)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return fail(format!("{}: {e}", binary.display())),
    };
    if !jsonl {
        println!("cargo atrium fuzz: {} ({}) with {}", t.name, t.function, binary.display());
    }
    let mut p = fuzz::Parse::default();
    let (mut ncrash, mut last) = (0u64, std::time::Instant::now() - std::time::Duration::from_secs(2));
    let stderr = child.stderr.take().expect("piped");
    for l in BufReader::new(stderr).lines().map_while(Result::ok) {
        match p.line(&l, &project) {
            Some(fuzz::Event::Progress { execs, corpus }) if last.elapsed().as_secs() >= 1 => {
                last = std::time::Instant::now();
                let secs = start.elapsed().as_secs();
                if jsonl {
                    println!("{}", serde_json::json!({"event": "progress", "execs": execs, "corpus": corpus, "secs": secs}));
                } else {
                    println!("  {execs} runs, corpus {corpus}, {secs} s");
                }
            }
            Some(fuzz::Event::Crash { input, summary, frame }) => {
                ncrash += 1;
                if jsonl {
                    println!("{}", serde_json::json!({
                        "event": "crash", "target": t.name, "input": input.display().to_string(),
                        "summary": summary, "frame": frame, "in_core": t.in_core(), "fix": fuzz::fix(t),
                        "binary": binary.display().to_string(),
                    }));
                } else {
                    println!("crash in {}: {summary}", t.name);
                    if let Some(f) = &frame {
                        println!("  at {f}");
                    }
                    println!("  input: {}\n  replay: {} {}", input.display(), binary.display(), input.display());
                    if let Some(f) = fuzz::fix(t) {
                        println!("  this input crashed the core, which holds the app's capabilities: {f}");
                    }
                }
            }
            _ => {}
        }
    }
    let _ = child.wait();
    if jsonl {
        println!("{}", serde_json::json!({"event": "done", "crashes": ncrash, "execs": p.execs, "secs": start.elapsed().as_secs()}));
    } else {
        println!("cargo atrium fuzz: {} runs, {ncrash} crash(es)", p.execs);
    }
    if ncrash > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn cmd_run(args: &[String]) -> ExitCode {
    let fail = |m: String| {
        eprintln!("cargo atrium run: {m}");
        ExitCode::FAILURE
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(mpath) = find_manifest(&cwd) else { return fail("no atrium.toml here or above".into()) };
    let proj = mpath.parent().unwrap_or(Path::new(".")).to_path_buf();
    let m = match std::fs::read_to_string(&mpath).map_err(|e| e.to_string())
        .and_then(|t| t.parse::<toml::Value>().map_err(|e| e.to_string())) {
        Ok(m) => m,
        Err(e) => return fail(format!("{}: {e}", mpath.display())),
    };
    let Some(entry) = m.get("app").and_then(|a| a.get("entry")).and_then(|e| e.as_str()).map(str::to_string) else {
        return fail("atrium.toml has no [app] entry".into());
    };
    if entry.starts_with('/') || entry.split('/').any(|c| c == ".." || c.is_empty()) {
        return fail(format!("entry {entry:?} isn't a path in the project"));
    }
    let _ = &m;
    let text = std::fs::read_to_string(&mpath).unwrap_or_default();
    let progs = match project_programs(&proj, &text) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let release = args.iter().any(|a| a == "--release");
    let prev = std::env::current_dir().unwrap_or_default();
    let _ = std::env::set_current_dir(&proj);
    let built = build_programs(&progs, release, None, args);
    let _ = std::env::set_current_dir(prev);
    let built = match built {
        Ok(b) => b,
        Err(e) => return fail(e),
    };
    for (entry, built) in &built {
        let dest = proj.join(entry);
        let copied = dest.parent().map(std::fs::create_dir_all).unwrap_or(Ok(()))
            .and_then(|_| std::fs::copy(built, &dest).map(|_| ()));
        if let Err(e) = copied {
            return fail(format!("{} -> {}: {e}", built.display(), dest.display()));
        }
    }
    // Run takes the project as a folder in ~/work.
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let real = std::fs::canonicalize(&proj).unwrap_or(proj.clone());
    let work = std::fs::canonicalize(home.join("work")).unwrap_or(home.join("work"));
    let Ok(rel) = real.strip_prefix(&work) else {
        return fail(format!("{} isn't in ~/work: Run takes a project in this workspace's work folder", real.display()));
    };
    let rel = if rel.as_os_str().is_empty() { ".".to_string() } else { rel.display().to_string() };
    let runner = ["/atrium/bin/atrium-run"].iter().map(PathBuf::from).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("atrium-run"));
    match Command::new(&runner).arg(&rel).status() {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(1, 255) as u8),
        Err(e) => fail(format!("{}: {e} (Run works in an Atrium workspace's Terminal)", runner.display())),
    }
}
