//! `cargo atrium fuzz` (app-composition.md §6, §6.1): libFuzzer harnesses
//! for a project's workers and the core's input surfaces, generated and
//! run in the project's own target folder, on stable Rust.
//!
//! Targets are declared in the project's Cargo.toml:
//!
//! ```toml
//! [package.metadata.atrium.fuzz."worker:decode"]
//! function = "photos::decode"        # fn(&[u8]) -> anything, in the lib
//! about = "the picture decoder"
//!
//! [package.metadata.atrium.fuzz."core:open"]
//! function = "photos::read_album"    # a surface of the core
//! ```
//!
//! Each `[[workers]]` entry in atrium.toml is a target too, `worker:<name>`,
//! with nothing to declare: its handler, `<lib>::workers::<name>` (dashes
//! as underscores), the same `fn(Req, Option<File>) -> Result<Resp,
//! String>` the worker's `atrium_worker::main!` serves. For `input = "fd"`
//! each input is the file and the request is `Req::default()`; for
//! `input = "message"` each input is a frame, decoded with its bounds
//! checked as the worker decodes it. Either way an answer that breaks its
//! own message's bounds (which the core would refuse) is a crash too. A
//! target declared in Cargo.toml under the same name wins.
//!
//! A `core:` target is an input surface of the process that holds the
//! capabilities: a crash there is the insecure pattern itself, and the
//! finding says to move that work into a worker.
//!
//! The harness is `fuzz_target!(|data| { let _ = function(data); })` in
//! `target/atrium-fuzz/<target>/`, built with libfuzzer-sys and the
//! sanitizer-coverage passes (stable codegen flags), with debug
//! assertions and overflow checks on. Corpus: `fuzz/corpus/<target>/`;
//! crashes: `fuzz/crashes/<target>/`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// What the fuzzed function takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `fn(&[u8])`, declared in Cargo.toml.
    Bytes,
    /// A worker's handler for `input = "fd"`: the bytes are the file.
    File,
    /// A worker's handler for `input = "message"`: the bytes are a frame.
    Frame,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub name:     String,
    pub function: String,
    pub about:    String,
    pub shape:    Shape,
}

impl Target {
    pub fn in_core(&self) -> bool { self.name.starts_with("core:") }
    pub fn kind(&self) -> &'static str { if self.in_core() { "core" } else { "worker" } }
    /// A file-name-safe form of the name (`worker:decode` → `worker-decode`).
    pub fn slug(&self) -> String {
        self.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
    }
    pub fn to_json(&self) -> Value { json!({"target": self.name, "kind": self.kind(), "about": self.about}) }
}

/// The lib crate's name as Cargo.toml says it (`[lib] name`, else the
/// package's, `-` as `_`).
pub fn lib_name_of(v: &toml::Value) -> Option<String> {
    v.get("lib").and_then(|l| l.get("name")).or_else(|| v.get("package").and_then(|p| p.get("name")))
        .and_then(|n| n.as_str()).map(|n| n.replace('-', "_"))
}

/// The project's targets: those Cargo.toml declares, then one for each
/// `[[workers]]` entry in atrium.toml (`atrium_toml`) not declared.
pub fn targets(cargo_toml: &str, atrium_toml: Option<&str>) -> Result<Vec<Target>, String> {
    let v: toml::Value = cargo_toml.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
    let mut out = declared(&v)?;
    let Some(m) = atrium_toml else { return Ok(out) };
    let m: toml::Value = m.parse().map_err(|e| format!("atrium.toml: {e}"))?;
    let Some(workers) = m.get("workers").and_then(|w| w.as_array()) else { return Ok(out) };
    let lib = lib_name_of(&v).ok_or("Cargo.toml: no package name")?;
    for w in workers {
        let Some(name) = w.get("name").and_then(|n| n.as_str()) else { continue };
        if name.is_empty() || name.len() > 48 || !name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-') {
            return Err(format!("atrium.toml: worker name {name:?} isn't [a-z0-9-]"));
        }
        let tname = format!("worker:{name}");
        if out.iter().any(|t| t.name == tname) {
            continue;
        }
        let shape = match w.get("input").and_then(|i| i.as_str()).unwrap_or("fd") {
            "message" => Shape::Frame,
            _ => Shape::File,
        };
        out.push(Target {
            function: format!("{lib}::workers::{}", name.replace('-', "_")),
            about: format!("the {name} worker ({})", if shape == Shape::File { "a file in" } else { "messages in" }),
            name: tname,
            shape,
        });
    }
    Ok(out)
}

/// The targets Cargo.toml declares.
fn declared(v: &toml::Value) -> Result<Vec<Target>, String> {
    let Some(t) = v.get("package").and_then(|p| p.get("metadata")).and_then(|m| m.get("atrium")).and_then(|a| a.get("fuzz")) else {
        return Ok(Vec::new());
    };
    let t = t.as_table().ok_or("package.metadata.atrium.fuzz: not a table")?;
    let mut out = Vec::new();
    for (name, e) in t {
        if !(name.starts_with("worker:") || name.starts_with("core:")) || name.len() > 64 {
            return Err(format!("fuzz target {name:?}: names are worker:<name> or core:<surface>"));
        }
        let function = e.get("function").and_then(|f| f.as_str()).ok_or_else(|| format!("fuzz target {name:?}: no function"))?;
        if function.is_empty() || !function.split("::").all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            return Err(format!("fuzz target {name:?}: function {function:?} isn't a path like mylib::decode"));
        }
        let about = e.get("about").and_then(|a| a.as_str()).unwrap_or_default().to_string();
        out.push(Target { name: name.clone(), function: function.to_string(), about, shape: Shape::Bytes });
    }
    Ok(out)
}

/// The libfuzzer-sys version the harness asks for.
pub const LIBFUZZER_SYS: &str = "0.4";

/// The coverage flags (what cargo-fuzz passes with `--sanitizer none`).
pub const RUSTFLAGS: &str = "--cfg fuzzing -Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=4 \
-Cllvm-args=-sanitizer-coverage-inline-8bit-counters -Cllvm-args=-sanitizer-coverage-pc-table \
-Cllvm-args=-sanitizer-coverage-trace-compares -Cdebug-assertions -Coverflow-checks";

/// The project's own `atrium-worker` dependency, for a harness of a
/// worker's handler: as written in its Cargo.toml, a relative `path` made
/// absolute.
pub fn worker_dependency(cargo_toml: &str, project: &Path) -> Option<String> {
    let v: toml::Value = cargo_toml.parse().ok()?;
    let d = v.get("dependencies")?.get("atrium-worker")?.clone();
    let d = match d {
        toml::Value::Table(mut t) => {
            if let Some(p) = t.get("path").and_then(|p| p.as_str()) {
                let abs = project.join(p).display().to_string();
                t.insert("path".into(), toml::Value::String(abs));
            }
            t.remove("optional");
            toml::Value::Table(t)
        }
        other => other,
    };
    // An inline table: `{ version = "0.1", registry = "atrium" }`.
    Some(match &d {
        toml::Value::Table(t) => {
            let kv: Vec<String> = t.iter().map(|(k, v)| format!("{k} = {v}")).collect();
            format!("{{ {} }}", kv.join(", "))
        }
        v => v.to_string(),
    })
}

/// The harness of a worker's handler.
const HANDLER_HARNESS: &str = r#"use atrium_worker::Message;
use std::io::{Seek, Write};

/// The answer as the core gets it: framed, then decoded with its bounds.
fn answered<Resp: Message>(r: Result<Resp, String>) {
    if let Ok(r) = r {
        if let Err(e) = Resp::from_frame(&r.to_frame(), usize::MAX) {
            panic!("the worker's answer breaks its own bounds, so the core would refuse it: {e}");
        }
    }
}

#[allow(dead_code)]
fn with_frame<Req: Message, Resp: Message>(f: fn(Req, Option<std::fs::File>) -> Result<Resp, String>, data: &[u8]) {
    if let Ok(req) = Req::from_frame(data, atrium_worker::DEFAULT_MAX) {
        answered(f(req, None));
    }
}

#[allow(dead_code)]
fn with_file<Req: Message + Default, Resp: Message>(f: fn(Req, Option<std::fs::File>) -> Result<Resp, String>, data: &[u8]) {
    thread_local! {
        static FILE: std::fs::File = {
            let p = std::env::temp_dir().join(format!("atrium-fuzz-{}", std::process::id()));
            let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).expect("a scratch file");
            let _ = std::fs::remove_file(&p);
            f
        };
    }
    let file = FILE.with(|f| {
        let mut f = f.try_clone().expect("the scratch file");
        f.set_len(0).and_then(|_| f.rewind()).and_then(|_| f.write_all(data)).and_then(|_| f.rewind()).expect("the scratch file");
        f
    });
    answered(f(Req::default(), Some(file)));
}
"#;

/// The harness crate: (Cargo.toml, src/main.rs). `worker_dep` is the
/// project's atrium-worker dependency ([`worker_dependency`]), which a
/// worker's handler target needs.
pub fn harness(t: &Target, lib_name: &str, package: &str, project: &Path, worker_dep: Option<&str>) -> Result<(String, String), String> {
    let pkg = format!("atrium-fuzz-{}", t.slug());
    let worker = match (t.shape, worker_dep) {
        (Shape::Bytes, _) => String::new(),
        (_, Some(d)) => format!("atrium-worker = {d}\n"),
        (_, None) => return Err(format!("{}: the worker's handler needs atrium-worker in the project's [dependencies]", t.name)),
    };
    let cargo = format!(
        "# Written by cargo atrium fuzz for {name}.\n[package]\nname = {pkg:?}\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [dependencies]\nlibfuzzer-sys = {LIBFUZZER_SYS:?}\n{lib_name} = {{ package = {package:?}, path = {project:?} }}\n{worker}\n\
         [[bin]]\nname = {pkg:?}\npath = \"src/main.rs\"\ntest = false\ndoc = false\n\n\
         [profile.release]\ndebug = 2\n\n[workspace]\n",
        name = t.name,
        project = project.display().to_string(),
    );
    let (helpers, call) = match t.shape {
        Shape::Bytes => ("", format!("let _ = {}(data);", t.function)),
        Shape::File => (HANDLER_HARNESS, format!("with_file({}, data);", t.function)),
        Shape::Frame => (HANDLER_HARNESS, format!("with_frame({}, data);", t.function)),
    };
    let main = format!(
        "#![no_main]\n// {name}: {about}\n{helpers}\nlibfuzzer_sys::fuzz_target!(|data: &[u8]| {{\n    {call}\n}});\n",
        name = t.name,
        about = t.about.replace('\n', " "),
    );
    Ok((cargo, main))
}

/// What the run has said so far.
#[derive(Debug, Default)]
pub struct Parse {
    pub execs:   u64,
    pub corpus:  u64,
    panic:       Option<String>,
    frame:       Option<String>,
    signal:      Option<String>,
}

/// An event for the caller to print.
#[derive(Debug, PartialEq)]
pub enum Event {
    Progress { execs: u64, corpus: u64 },
    Crash { input: PathBuf, summary: String, frame: Option<String> },
}

impl Parse {
    /// One line of libFuzzer's stderr. `project` turns absolute source
    /// paths into the project's own.
    pub fn line(&mut self, l: &str, project: &Path) -> Option<Event> {
        // "#1234\tNEW    cov: 56 ft: 78 corp: 9/120b lim: 4 exec/s: 0 rss: 30Mb"
        if let Some(rest) = l.strip_prefix('#') {
            let mut w = rest.split_whitespace();
            let n: u64 = w.next()?.parse().ok()?;
            self.execs = n;
            if let Some(i) = l.find("corp: ") {
                self.corpus = l[i + 6..].split('/').next().and_then(|c| c.parse().ok()).unwrap_or(self.corpus);
            }
            return Some(Event::Progress { execs: self.execs, corpus: self.corpus });
        }
        // "thread '<unnamed>' panicked at src/lib.rs:5:9:" then the message.
        if let Some(i) = l.find("panicked at ") {
            let at = l[i + 12..].trim_end_matches(':');
            let mut parts = at.rsplitn(3, ':');
            let (_col, line, file) = (parts.next(), parts.next(), parts.next());
            // A relative path is the harness's own src/main.rs (the
            // project is a path dependency, so its files come absolute):
            // no frame of the project's.
            if let (Some(file), Some(line)) = (file.filter(|f| Path::new(f).is_absolute()), line) {
                let file = Path::new(file).strip_prefix(project).map(|p| p.display().to_string()).unwrap_or_else(|_| file.to_string());
                self.frame = Some(format!("{file}:{line}"));
            }
            self.panic = Some(String::new());
            return None;
        }
        if let Some(p) = &mut self.panic {
            if p.is_empty() && !l.starts_with("note:") && !l.starts_with("==") && !l.trim().is_empty() {
                *p = l.trim().to_string();
            }
        }
        if l.contains("ERROR: libFuzzer: ") {
            self.signal = l.split("ERROR: libFuzzer: ").nth(1).map(|s| s.trim().to_string());
        }
        // "artifact_prefix='...'; Test unit written to fuzz/crashes/x/crash-<sha1>"
        if let Some(i) = l.find("Test unit written to ") {
            let input = PathBuf::from(l[i + 21..].trim());
            let summary = match (&self.panic, &self.signal) {
                (Some(p), _) if !p.is_empty() => format!("panic: {p}"),
                (_, Some(s)) => s.clone(),
                _ => "crash".into(),
            };
            let frame = self.frame.clone();
            *self = Parse { execs: self.execs, corpus: self.corpus, ..Default::default() };
            return Some(Event::Crash { input, summary, frame });
        }
        None
    }
}

/// The finding's fix, for a crash in the core.
pub fn fix(t: &Target) -> Option<String> {
    t.in_core().then(|| "Move into a worker (template: viewer)".to_string())
}

/// The project's lib crate name (`[lib] name` or the package name with
/// `-` → `_`), from `cargo metadata --no-deps`.
pub fn lib_name(metadata: &Value, manifest: &Path) -> Option<String> {
    let mp = manifest.display().to_string();
    let pkg = metadata["packages"].as_array()?.iter().find(|p| p["manifest_path"].as_str() == Some(mp.as_str()))?;
    let lib = pkg["targets"].as_array()?.iter().find(|t| t["kind"].as_array().is_some_and(|k| k.iter().any(|k| k == "lib" || k == "rlib")))?;
    Some(lib["name"].as_str()?.replace('-', "_"))
}

/// Flags for the fuzzer binary.
pub fn fuzzer_args(corpus: &Path, crashes: &Path, max_time: Option<u64>) -> Vec<String> {
    let mut v = vec![
        format!("-artifact_prefix={}/", crashes.display()),
        "-print_final_stats=1".into(),
    ];
    if let Some(s) = max_time {
        v.push(format!("-max_total_time={s}"));
    }
    v.push(corpus.display().to_string());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_targets() {
        let t = targets(
            "[package]\nname = \"photos\"\n[package.metadata.atrium.fuzz.\"worker:decode\"]\nfunction = \"photos::decode\"\nabout = \"pictures\"\n\
             [package.metadata.atrium.fuzz.\"core:open\"]\nfunction = \"photos::album::read\"\n",
            None,
        )
        .unwrap();
        assert_eq!(t.len(), 2);
        let core = t.iter().find(|x| x.name == "core:open").unwrap();
        assert!(core.in_core());
        assert_eq!(core.slug(), "core-open");
        assert_eq!(fix(core).as_deref(), Some("Move into a worker (template: viewer)"));
        assert!(targets("[package.metadata.atrium.fuzz.\"x\"]\nfunction = \"a::b\"\n", None).is_err());
        assert!(targets("[package.metadata.atrium.fuzz.\"core:x\"]\nfunction = \"a::b; evil()\"\n", None).is_err());
        assert!(targets("[package]\nname = \"x\"\n", None).unwrap().is_empty());
    }

    #[test]
    fn libfuzzer_output() {
        let pr = Path::new("/w/photos");
        let mut p = Parse::default();
        assert_eq!(
            p.line("#4096\tpulse  cov: 12 ft: 14 corp: 5/37b lim: 43 exec/s: 2048 rss: 31Mb", pr),
            Some(Event::Progress { execs: 4096, corpus: 5 })
        );
        assert_eq!(p.line("thread '<unnamed>' panicked at src/main.rs:10:13:", pr), None);
        assert_eq!(p.frame, None);
        assert_eq!(p.line("thread '<unnamed>' panicked at /w/photos/src/lib.rs:7:13:", pr), None);
        assert_eq!(p.line("index out of bounds: the len is 3 but the index is 9", pr), None);
        assert_eq!(p.line("==123== ERROR: libFuzzer: deadly signal", pr), None);
        let e = p.line("artifact_prefix='fuzz/crashes/core-open/'; Test unit written to fuzz/crashes/core-open/crash-ab12", pr);
        assert_eq!(
            e,
            Some(Event::Crash {
                input: "fuzz/crashes/core-open/crash-ab12".into(),
                summary: "panic: index out of bounds: the len is 3 but the index is 9".into(),
                frame: Some("src/lib.rs:7".into()),
            })
        );
    }

    #[test]
    fn harness_text() {
        let t = Target { name: "worker:decode".into(), function: "photos::decode".into(), about: "x".into(), shape: Shape::Bytes };
        let (c, m) = harness(&t, "photos", "photos", Path::new("/w/photos"), None).unwrap();
        assert!(c.contains("photos = { package = \"photos\", path = \"/w/photos\" }"), "{c}");
        assert!(c.contains("[workspace]") && !c.contains("atrium-worker"));
        assert!(m.contains("let _ = photos::decode(data);"));
    }

    #[test]
    fn worker_targets() {
        let cargo = "[package]\nname = \"my-photos\"\n[dependencies]\natrium-worker = { path = \"../sdk/atrium-worker\" }\n\
                     [package.metadata.atrium.fuzz.\"worker:thumb\"]\nfunction = \"my_photos::thumb_bytes\"\n";
        let m = "[app]\nid = \"x\"\n[[workers]]\nname = \"decode\"\nentry = \"bin/d\"\n\
                 [[workers]]\nname = \"tag-index\"\nentry = \"bin/i\"\ninput = \"message\"\n\
                 [[workers]]\nname = \"thumb\"\nentry = \"bin/t\"\n";
        let t = targets(cargo, Some(m)).unwrap();
        let names: Vec<_> = t.iter().map(|t| (t.name.as_str(), t.function.as_str(), t.shape)).collect();
        assert_eq!(names, [
            ("worker:thumb", "my_photos::thumb_bytes", Shape::Bytes),
            ("worker:decode", "my_photos::workers::decode", Shape::File),
            ("worker:tag-index", "my_photos::workers::tag_index", Shape::Frame),
        ]);
        let dep = worker_dependency(cargo, Path::new("/w/photos")).unwrap();
        assert_eq!(dep, "{ path = \"/w/photos/../sdk/atrium-worker\" }");
        let (c, main) = harness(&t[1], "my_photos", "my-photos", Path::new("/w/photos"), Some(&dep)).unwrap();
        assert!(c.contains("atrium-worker = { path = "), "{c}");
        assert!(main.contains("with_file(my_photos::workers::decode, data);"));
        let (_, main) = harness(&t[2], "my_photos", "my-photos", Path::new("/w/photos"), Some(&dep)).unwrap();
        assert!(main.contains("with_frame(my_photos::workers::tag_index, data);"));
        assert!(harness(&t[2], "my_photos", "my-photos", Path::new("/w/photos"), None).is_err());
        let reg = worker_dependency("[dependencies]\natrium-worker = { version = \"0.1\", registry = \"atrium\" }\n", Path::new("/w")).unwrap();
        assert_eq!(reg, "{ registry = \"atrium\", version = \"0.1\" }");
        assert!(targets(cargo, Some("[[workers]]\nname = \"Bad Name\"\nentry = \"x\"\n")).is_err());
    }
}
