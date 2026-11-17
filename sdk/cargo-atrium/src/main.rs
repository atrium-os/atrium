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
  registry verify <folder or https URL> [--keys <dir>]
      Check the SDK registry's index against the platform keys
      (default /etc/atrium/platform-keys): index.sig over index.files,
      and every listed file.
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
            println!("  next: cd {dir} && cargo atrium check");
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

fn read_keys(dir: &Path) -> Result<Vec<String>, String> {
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
