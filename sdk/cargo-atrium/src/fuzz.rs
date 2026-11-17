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

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub name:     String,
    pub function: String,
    pub about:    String,
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

/// The targets a Cargo.toml declares.
pub fn targets(cargo_toml: &str) -> Result<Vec<Target>, String> {
    let v: toml::Value = cargo_toml.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
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
        out.push(Target { name: name.clone(), function: function.to_string(), about });
    }
    Ok(out)
}

/// The libfuzzer-sys version the harness asks for.
pub const LIBFUZZER_SYS: &str = "0.4";

/// The coverage flags (what cargo-fuzz passes with `--sanitizer none`).
pub const RUSTFLAGS: &str = "--cfg fuzzing -Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=4 \
-Cllvm-args=-sanitizer-coverage-inline-8bit-counters -Cllvm-args=-sanitizer-coverage-pc-table \
-Cllvm-args=-sanitizer-coverage-trace-compares -Cdebug-assertions -Coverflow-checks";

/// The harness crate: (Cargo.toml, src/main.rs).
pub fn harness(t: &Target, lib_name: &str, project: &Path) -> (String, String) {
    let pkg = format!("atrium-fuzz-{}", t.slug());
    let cargo = format!(
        "# Written by cargo atrium fuzz for {name}.\n[package]\nname = {pkg:?}\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [dependencies]\nlibfuzzer-sys = {LIBFUZZER_SYS:?}\n{lib_name} = {{ path = {project:?} }}\n\n\
         [[bin]]\nname = {pkg:?}\npath = \"src/main.rs\"\ntest = false\ndoc = false\n\n\
         [profile.release]\ndebug = 2\n\n[workspace]\n",
        name = t.name,
        project = project.display().to_string(),
    );
    let main = format!(
        "#![no_main]\n// {name}: {about}\nlibfuzzer_sys::fuzz_target!(|data: &[u8]| {{\n    let _ = {f}(data);\n}});\n",
        name = t.name,
        about = t.about.replace('\n', " "),
        f = t.function,
    );
    (cargo, main)
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
            if let (Some(file), Some(line)) = (file, line) {
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
        )
        .unwrap();
        assert_eq!(t.len(), 2);
        let core = t.iter().find(|x| x.name == "core:open").unwrap();
        assert!(core.in_core());
        assert_eq!(core.slug(), "core-open");
        assert_eq!(fix(core).as_deref(), Some("Move into a worker (template: viewer)"));
        assert!(targets("[package.metadata.atrium.fuzz.\"x\"]\nfunction = \"a::b\"\n").is_err());
        assert!(targets("[package.metadata.atrium.fuzz.\"core:x\"]\nfunction = \"a::b; evil()\"\n").is_err());
        assert!(targets("[package]\nname = \"x\"\n").unwrap().is_empty());
    }

    #[test]
    fn libfuzzer_output() {
        let pr = Path::new("/w/photos");
        let mut p = Parse::default();
        assert_eq!(
            p.line("#4096\tpulse  cov: 12 ft: 14 corp: 5/37b lim: 43 exec/s: 2048 rss: 31Mb", pr),
            Some(Event::Progress { execs: 4096, corpus: 5 })
        );
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
        let t = Target { name: "worker:decode".into(), function: "photos::decode".into(), about: "x".into() };
        let (c, m) = harness(&t, "photos", Path::new("/w/photos"));
        assert!(c.contains("photos = { path = \"/w/photos\" }"), "{c}");
        assert!(c.contains("[workspace]"));
        assert!(m.contains("let _ = photos::decode(data);"));
    }
}
