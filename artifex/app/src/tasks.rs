//! Tasks (dev-tools.md §3.4): Build, Test and Run found from the project's
//! own build files, run in the workspace with `WorkspaceExec`, and the
//! compiler's `file:line:col: error: …` lines turned into problems the
//! editor can jump to.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Build,
    Test,
    Run,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Build => "Build",
            Kind::Test => "Test",
            Kind::Run => "Run",
        }
    }
}

/// A command the project's build files offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub kind: Kind,
    /// What the button says, e.g. "Build (cargo)".
    pub label: String,
    pub argv: Vec<String>,
    /// Folder in `work` it runs in.
    pub cwd: String,
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// The tasks for project folder `project` (relative to `work`, "" for
/// `work` itself), looking at what's in `dir` (that folder as Artifex sees
/// it).
pub fn detect(dir: &Path, project: &str) -> Vec<Task> {
    let has = |f: &str| dir.join(f).is_file();
    let mk = |kind: Kind, tool: &str, argv: Vec<String>| Task {
        kind,
        label: format!("{} ({tool})", kind.label()),
        argv,
        cwd: project.to_string(),
    };
    let mut out = Vec::new();
    if has("Cargo.toml") {
        // Short messages: one `file:line:col: error: …` line each.
        out.push(mk(Kind::Build, "cargo", argv(&["cargo", "build", "--message-format=short"])));
        out.push(mk(Kind::Test, "cargo", argv(&["cargo", "test", "--message-format=short"])));
        out.push(mk(Kind::Run, "cargo", argv(&["cargo", "run", "--message-format=short"])));
    } else if has("meson.build") {
        let setup = "[ -d build ] || meson setup build";
        out.push(mk(Kind::Build, "meson", sh(&format!("{setup} && meson compile -C build"))));
        out.push(mk(Kind::Test, "meson", sh(&format!("{setup} && meson test -C build"))));
    } else if has("CMakeLists.txt") {
        let setup = "[ -d build ] || cmake -S . -B build";
        out.push(mk(Kind::Build, "cmake", sh(&format!("{setup} && cmake --build build"))));
        out.push(mk(Kind::Test, "cmake", sh(&format!("{setup} && cmake --build build && ctest --test-dir build"))));
    } else if has("Makefile") || has("makefile") || has("BSDmakefile") || has("GNUmakefile") {
        let tool = if has("GNUmakefile") { "gmake" } else { "make" };
        out.push(mk(Kind::Build, tool, argv(&[tool])));
        out.push(mk(Kind::Test, tool, argv(&[tool, "test"])));
    } else if let Some(c) = single_c(dir) {
        // One C file and no build system: what the Scratch template is.
        out.push(mk(Kind::Build, "cc", argv(&["cc", "-Wall", "-g", "-o", &stem(&c), &c])));
        out.push(mk(Kind::Run, "cc", sh(&format!("cc -Wall -g -o {0} {1} && ./{0}", stem(&c), c))));
    }
    out
}

fn single_c(dir: &Path) -> Option<String> {
    let mut found = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.ends_with(".c") && e.path().is_file() {
            if found.is_some() {
                return None;
            }
            found = Some(name);
        }
    }
    found.filter(|n| n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'))
}

fn stem(file: &str) -> String {
    file.trim_end_matches(".c").to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// A compiler message pointing at a place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// As the compiler wrote it: relative to the task's folder, or absolute
    /// in the workspace.
    pub file: String,
    /// 1-based, as compilers print them.
    pub line: usize,
    pub col: usize,
    pub severity: Severity,
    pub message: String,
}

/// The problem a line of output names, in the `file:line[:col]: severity:
/// message` shape cc, clang, gcc, rustc (`--message-format=short`) and
/// meson print.
pub fn parse_problem(line: &str) -> Option<Problem> {
    let line = line.trim_end();
    for (key, severity) in [(": error", Severity::Error), (": warning", Severity::Warning), (": note", Severity::Note), (": fatal error", Severity::Error)] {
        let Some(at) = line.find(key) else { continue };
        let head = &line[..at];
        let rest = &line[at + key.len()..];
        // `error[E0425]: msg` or `error: msg`.
        let message = match rest.find(": ") {
            Some(i) if rest[..i].chars().all(|c| c != ' ') => rest[i + 2..].to_string(),
            _ => continue,
        };
        let mut parts = head.rsplitn(3, ':');
        let a = parts.next()?;
        let b = parts.next()?;
        let (file, line_no, col): (&str, usize, usize) = match parts.next() {
            Some(f) if a.parse::<usize>().is_ok() && b.parse::<usize>().is_ok() => (f, b.parse().ok()?, a.parse().ok()?),
            other => {
                // `file:line: error:` (no column).
                let f = match other {
                    Some(f) => format!("{f}:{b}"),
                    None => b.to_string(),
                };
                let line_no: usize = a.parse().ok()?;
                return (!f.is_empty() && !f.contains(' ')).then(|| Problem { file: f, line: line_no, col: 1, severity, message });
            }
        };
        if file.is_empty() || file.contains(' ') || line_no == 0 {
            return None;
        }
        return Some(Problem { file: file.to_string(), line: line_no, col: col.max(1), severity, message });
    }
    None
}

/// All the problems in a task's output, first of each place only.
pub fn problems(output: &str) -> Vec<Problem> {
    let mut out: Vec<Problem> = Vec::new();
    for l in output.lines() {
        if let Some(p) = parse_problem(l) {
            if !out.iter().any(|q| q.file == p.file && q.line == p.line && q.col == p.col && q.message == p.message) {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_from_compilers() {
        let p = parse_problem("src/main.rs:3:5: error[E0425]: cannot find value `x` in this scope").unwrap();
        assert_eq!((p.file.as_str(), p.line, p.col, p.severity), ("src/main.rs", 3, 5, Severity::Error));
        assert_eq!(p.message, "cannot find value `x` in this scope");
        let p = parse_problem("hello.c:4:12: warning: unused variable 'y' [-Wunused-variable]").unwrap();
        assert_eq!((p.line, p.col, p.severity), (4, 12, Severity::Warning));
        let p = parse_problem("hello.c:9: error: expected ';'").unwrap();
        assert_eq!((p.file.as_str(), p.line, p.col), ("hello.c", 9, 1));
        let p = parse_problem("../lib/x.h:1:1: fatal error: 'y.h' file not found").unwrap();
        assert_eq!(p.severity, Severity::Error);
        assert!(parse_problem("   Compiling foo v0.1.0").is_none());
        assert!(parse_problem("error: could not compile `foo` (bin \"foo\") due to 1 previous error").is_none());
        assert!(parse_problem("warning: unused import").is_none());
        assert!(parse_problem("a b:1:2: error: x").is_none());
        let all = problems("a.c:1:1: error: x\na.c:1:1: error: x\nb.c:2:3: note: y\n");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn tasks_from_build_files() {
        let d = std::env::temp_dir().join(format!("artifex-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(detect(&d, "").is_empty());
        std::fs::write(d.join("hello.c"), "int main(){}").unwrap();
        let t = detect(&d, "app");
        assert_eq!(t[0].argv, vec!["cc", "-Wall", "-g", "-o", "hello", "hello.c"]);
        assert_eq!(t[0].cwd, "app");
        std::fs::write(d.join("other.c"), "").unwrap();
        assert!(detect(&d, "").is_empty());
        std::fs::write(d.join("Makefile"), "").unwrap();
        assert_eq!(detect(&d, "")[0].label, "Build (make)");
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        let t = detect(&d, "");
        assert_eq!(t.iter().map(|t| t.kind).collect::<Vec<_>>(), vec![Kind::Build, Kind::Test, Kind::Run]);
        assert_eq!(t[0].argv[0], "cargo");
        let _ = std::fs::remove_dir_all(&d);
    }
}
