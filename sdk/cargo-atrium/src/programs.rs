//! Where an app's programs are built: the core and each worker, which
//! `run`, `bundle` and `check` all need.
//!
//! A program's binary is found, in this order:
//!
//! 1. in another crate that `Cargo.toml` names for that worker:
//!
//!    ```toml
//!    [package.metadata.atrium.workers]
//!    parse = "../mail-parse"      # its folder, or its Cargo.toml
//!    ```
//!
//!    for a worker the app doesn't link (Mail's parse jail talks to the
//!    engine over a socket); it builds in that crate's own workspace;
//! 2. among the app's own packages (the workspace's members);
//! 3. in a crate the app depends on (Write's `officina-convert`), built
//!    with `-p <package>` in the app's workspace.
//!
//! The binary's name is the entry's file name, as before.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A binary of the app's own packages.
    Here,
    /// A binary of a package the app depends on.
    Dependency(String),
    /// A binary of a crate elsewhere: its Cargo.toml.
    Elsewhere(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// Where it goes in the bundle (the manifest's entry).
    pub entry: String,
    /// The cargo binary.
    pub bin: String,
    /// The worker's name; None for the core.
    pub worker: Option<String>,
    /// Where it is built; None when nothing has it.
    pub source: Option<Source>,
}

/// The binaries of a Cargo.toml without running cargo: its `[[bin]]`
/// names, the package's own name for `src/main.rs`, and `src/bin/*.rs`.
pub fn bins_of(manifest: &Path) -> Result<BTreeSet<String>, String> {
    let text = std::fs::read_to_string(manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let v: toml::Value = text.parse().map_err(|e| format!("{}: {e}", manifest.display()))?;
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let mut out = BTreeSet::new();
    for b in v.get("bin").and_then(|b| b.as_array()).into_iter().flatten() {
        if let Some(n) = b.get("name").and_then(|n| n.as_str()) {
            out.insert(n.to_string());
        }
    }
    let autobins = v.get("package").and_then(|p| p.get("autobins")).and_then(|a| a.as_bool()) != Some(false);
    if autobins {
        if let Some(n) = v.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) {
            if dir.join("src/main.rs").is_file() {
                out.insert(n.to_string());
            }
        }
        if let Ok(rd) = std::fs::read_dir(dir.join("src/bin")) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rs") {
                    if let Some(s) = p.file_stem() {
                        out.insert(s.to_string_lossy().into_owned());
                    }
                } else if p.join("main.rs").is_file() {
                    out.insert(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    Ok(out)
}

/// The workers `Cargo.toml` says are built elsewhere: name → that crate's
/// Cargo.toml.
pub fn configured(project: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let p = project.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&p) else { return Ok(Vec::new()) };
    let v: toml::Value = text.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
    let Some(t) = v.get("package").and_then(|p| p.get("metadata")).and_then(|m| m.get("atrium"))
        .and_then(|a| a.get("workers")).and_then(|w| w.as_table()) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for (name, path) in t {
        let path = path.as_str().ok_or_else(|| format!("Cargo.toml: package.metadata.atrium.workers.{name}: a path"))?;
        let mut m = project.join(path);
        if !m.ends_with("Cargo.toml") {
            m = m.join("Cargo.toml");
        }
        out.push((name.clone(), m));
    }
    Ok(out)
}

/// The app's programs and where each is built. `metadata` is `cargo
/// metadata --format-version 1` (with dependencies) for the project, or
/// None for a project without Cargo.toml.
pub fn locate(project: &Path, atrium_toml: &str, metadata: Option<&Value>) -> Result<Vec<Program>, String> {
    let m: toml::Value = atrium_toml.parse().map_err(|e| format!("atrium.toml: {e}"))?;
    let core = m.get("bundle").and_then(|b| b.get("entry"))
        .or_else(|| m.get("app").and_then(|a| a.get("entry")))
        .and_then(|e| e.as_str()).ok_or("atrium.toml has no [app] entry")?;
    let mut progs = vec![(core.to_string(), None)];
    for w in m.get("workers").and_then(|w| w.as_array()).into_iter().flatten() {
        if let Some(e) = w.get("entry").and_then(|e| e.as_str()) {
            progs.push((e.to_string(), w.get("name").and_then(|n| n.as_str()).map(str::to_string)));
        }
    }
    for (e, _) in &progs {
        if !crate::bundle::inside(e) {
            return Err(format!("entry {e:?} isn't a path in the bundle"));
        }
    }
    let elsewhere = configured(project)?;
    let (mut here, mut deps): (BTreeSet<String>, Vec<(String, String)>) = (BTreeSet::new(), Vec::new());
    if let Some(md) = metadata {
        let members: BTreeSet<&str> = md.get("workspace_members").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).collect();
        for p in md.get("packages").and_then(Value::as_array).into_iter().flatten() {
            let id = p.get("id").and_then(Value::as_str).unwrap_or("");
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            for t in p.get("targets").and_then(Value::as_array).into_iter().flatten() {
                let is_bin = t.get("kind").and_then(Value::as_array).is_some_and(|k| k.iter().any(|x| x == "bin"));
                let Some(n) = t.get("name").and_then(Value::as_str).filter(|_| is_bin) else { continue };
                if members.contains(id) {
                    here.insert(n.to_string());
                } else {
                    deps.push((n.to_string(), name.to_string()));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (entry, worker) in progs {
        let bin = entry.rsplit('/').next().unwrap_or(&entry).to_string();
        let mut source = None;
        if let Some((_, manifest)) = worker.as_ref().and_then(|w| elsewhere.iter().find(|(n, _)| n == w)) {
            if !bins_of(manifest)?.contains(&bin) {
                return Err(format!("{} has no binary {bin:?} (the {} worker's entry)", manifest.display(), worker.as_deref().unwrap_or("")));
            }
            source = Some(Source::Elsewhere(manifest.clone()));
        } else if here.contains(&bin) {
            source = Some(Source::Here);
        } else if let Some((_, pkg)) = deps.iter().find(|(b, _)| *b == bin) {
            source = Some(Source::Dependency(pkg.clone()));
        }
        out.push(Program { entry, bin, worker, source });
    }
    Ok(out)
}

/// One `cargo build`: where it runs (`manifest`, None for the project)
/// and its arguments after `build`.
#[derive(Debug, PartialEq)]
pub struct Build {
    pub manifest: Option<PathBuf>,
    pub args: Vec<String>,
}

/// The builds that make every program: one for the project's own and its
/// dependencies' binaries, one per crate elsewhere.
pub fn builds(progs: &[Program]) -> Vec<Build> {
    let mut here = Vec::new();
    let mut out: Vec<Build> = Vec::new();
    for p in progs {
        match &p.source {
            Some(Source::Here) | None => here.extend(["--bin".to_string(), p.bin.clone()]),
            Some(Source::Dependency(pkg)) => out.push(Build { manifest: None, args: vec!["-p".into(), pkg.clone(), "--bin".into(), p.bin.clone()] }),
            Some(Source::Elsewhere(m)) => match out.iter_mut().find(|b| b.manifest.as_ref() == Some(m)) {
                Some(b) => b.args.extend(["--bin".to_string(), p.bin.clone()]),
                None => out.push(Build { manifest: Some(m.clone()), args: vec!["--bin".into(), p.bin.clone()] }),
            },
        }
    }
    if !here.is_empty() {
        out.insert(0, Build { manifest: None, args: here });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_workers_in_other_crates() {
        let dir = std::env::temp_dir().join(format!("atrium-programs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::create_dir_all(dir.join("parse/src/bin")).unwrap();
        std::fs::write(dir.join("parse/Cargo.toml"), "[package]\nname = \"mail-parse\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(dir.join("parse/src/bin/mail-parse-jail.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[package.metadata.atrium.workers]\nparse = \"../parse\"\n").unwrap();
        let m = "[app]\nid = \"org.x.app\"\nname = \"A\"\nversion = \"1\"\nentry = \"bin/app\"\n\n\
                 [[workers]]\nname = \"parse\"\nentry = \"bin/mail-parse-jail\"\n\n\
                 [[workers]]\nname = \"convert\"\nentry = \"bin/officina-convert\"\n\n\
                 [[workers]]\nname = \"gone\"\nentry = \"bin/nowhere\"\n";
        let md = serde_json::json!({
            "workspace_members": ["app 0.1.0"],
            "packages": [
                {"id": "app 0.1.0", "name": "app", "targets": [{"kind": ["bin"], "name": "app"}, {"kind": ["lib"], "name": "app"}]},
                {"id": "officina-convert 0.1.0", "name": "officina-convert", "targets": [{"kind": ["bin"], "name": "officina-convert"}]}
            ]
        });
        let p = locate(&dir.join("app"), m, Some(&md)).unwrap();
        let src: Vec<Option<Source>> = p.iter().map(|x| x.source.clone()).collect();
        assert_eq!(src, [
            Some(Source::Here),
            Some(Source::Elsewhere(dir.join("app/../parse/Cargo.toml"))),
            Some(Source::Dependency("officina-convert".into())),
            None,
        ]);
        let b = builds(&p);
        assert_eq!(b[0], Build { manifest: None, args: vec!["--bin".into(), "app".into(), "--bin".into(), "nowhere".into()] });
        assert_eq!(b[1].manifest, Some(dir.join("app/../parse/Cargo.toml")));
        assert_eq!(b[2].args, ["-p", "officina-convert", "--bin", "officina-convert"]);
        // A configured crate without that binary is an error, not a guess.
        let e = locate(&dir.join("app"), &m.replace("mail-parse-jail", "mail-parse-x"), Some(&md)).unwrap_err();
        assert!(e.contains("no binary \"mail-parse-x\""), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
