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
    // The core, then each worker: (bin, where it goes in the project).
    let mut entries = vec![entry];
    for w in m.get("workers").and_then(|w| w.as_array()).into_iter().flatten() {
        if let Some(e) = w.get("entry").and_then(|e| e.as_str()) {
            if e.starts_with('/') || e.split('/').any(|c| c == ".." || c.is_empty()) {
                return fail(format!("worker entry {e:?} isn't a path in the project"));
            }
            entries.push(e.to_string());
        }
    }
    let bins: Vec<(String, String)> = entries.iter().map(|e| (e.rsplit('/').next().unwrap_or(e).to_string(), e.clone())).collect();
    let release = args.iter().any(|a| a == "--release");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(&cargo);
    build.arg("build");
    for (b, _) in &bins {
        build.args(["--bin", b]);
    }
    match build.args(args).current_dir(&proj).status() {
        Ok(s) if s.success() => {}
        _ => return fail("build failed".into()),
    }
    let tdir = Command::new(&cargo).args(["metadata", "--no-deps", "--format-version", "1"]).current_dir(&proj).output().ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v["target_directory"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| proj.join("target"));
    for (bin, entry) in &bins {
        let built = tdir.join(if release { "release" } else { "debug" }).join(bin);
        let dest = proj.join(entry);
        let copied = dest.parent().map(std::fs::create_dir_all).unwrap_or(Ok(()))
            .and_then(|_| std::fs::copy(&built, &dest).map(|_| ()));
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
