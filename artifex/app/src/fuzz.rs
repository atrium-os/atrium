//! The fuzz panel (app-composition.md §6.1, plan M12): `cargo atrium fuzz`
//! runs in the workspace in the background, a target at a time — every
//! worker, and the core's own input surfaces. A crash in a worker is a bug
//! to fix; a crash in the core is the pattern app-composition.md exists to
//! avoid, flagged with what to do ("Move into a worker").
//!
//! The format (agreed with the SDK's `cargo atrium`): `--list --json` gives
//! the targets; a run with `--json` prints one JSON object a line:
//! `progress`, `crash`, `done`, and `error` when the target doesn't build.

use serde_json::Value;

pub const LIST_ARGV: [&str; 5] = ["cargo", "atrium", "fuzz", "--list", "--json"];

/// Seconds each target runs in a background round.
pub const ROUND_SECS: u32 = 120;

pub fn run_argv(target: &str) -> Vec<String> {
    ["cargo", "atrium", "fuzz", target, "--json", "--max-time", &ROUND_SECS.to_string()].iter().map(|a| a.to_string()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    /// A worker (holds nothing) or one of the core's input surfaces.
    pub core: bool,
    pub about: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    pub target: String,
    /// The input that crashes it, in the project.
    pub input: String,
    pub summary: String,
    /// `file:line` in the project, when known.
    pub frame: Option<String>,
    pub in_core: bool,
    pub fix: Option<String>,
    /// The fuzz binary, for replaying the input under the debugger.
    pub binary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fuzz {
    pub targets: Vec<Target>,
    pub listed: bool,
    /// Why there's no fuzzing (no cargo atrium, no targets).
    pub error: Option<String>,
    pub running: Option<String>,
    pub execs: u64,
    pub crashes: Vec<Crash>,
    /// Next target of the round.
    pub next: usize,
    pub on: bool,
    /// The last run's failure (a fuzz target that doesn't build), shown
    /// until a run of that target gets going.
    pub run_error: Option<(String, String)>,
}

pub fn targets(out: &[u8]) -> Result<Vec<Target>, String> {
    let text = String::from_utf8_lossy(out);
    let Some(i) = text.find('[') else {
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("no answer");
        return Err(if first.contains("no such command") || first.contains("unknown") {
            "cargo atrium fuzz isn't in this workspace yet (the SDK's Rust layer brings it)".into()
        } else {
            first.to_string()
        });
    };
    let v: Value = serde_json::from_str(&text[i..]).map_err(|e| e.to_string())?;
    Ok(v.as_array().map(|a| a.iter().filter_map(|t| {
        let name = t.get("target")?.as_str()?.to_string();
        let core = t.get("kind").and_then(Value::as_str).map_or(name.starts_with("core:"), |k| k == "core");
        Some(Target { name, core, about: t.get("about").and_then(Value::as_str).unwrap_or("").to_string() })
    }).collect()).unwrap_or_default())
}

impl Fuzz {
    /// One line of a run's output.
    pub fn line(&mut self, target: &str, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return };
        match v.get("event").and_then(Value::as_str) {
            Some("error") => {
                let m = v.get("message").and_then(Value::as_str).unwrap_or("failed").chars().take(300).collect();
                self.run_error = Some((target.to_string(), m));
            }
            Some("progress") => {
                if self.run_error.as_ref().is_some_and(|(t, _)| t == target) {
                    self.run_error = None;
                }
                self.execs = v.get("execs").and_then(Value::as_u64).unwrap_or(self.execs);
            }
            Some("crash") => {
                let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
                let c = Crash {
                    target: s("target").unwrap_or_else(|| target.to_string()),
                    input: s("input").unwrap_or_default(),
                    summary: s("summary").unwrap_or_else(|| "crashed".into()).chars().take(300).collect(),
                    frame: s("frame"),
                    in_core: v.get("in_core").and_then(Value::as_bool).unwrap_or(target.starts_with("core:")),
                    fix: s("fix"),
                    binary: s("binary"),
                };
                // One entry per place: a fuzzer finds the same bug many times.
                if !self.crashes.iter().any(|x| x.target == c.target && x.frame == c.frame && x.summary == c.summary) {
                    self.crashes.push(c);
                    self.crashes.sort_by_key(|c| !c.in_core);
                }
            }
            _ => {}
        }
    }

    /// The target to run next in the round, if fuzzing is on.
    pub fn next_target(&mut self) -> Option<String> {
        if !self.on || self.targets.is_empty() || self.running.is_some() {
            return None;
        }
        let t = self.targets[self.next % self.targets.len()].name.clone();
        self.next = (self.next + 1) % self.targets.len();
        Some(t)
    }

    /// Lines for the panel: (is a crash in the core, text, crash index).
    pub fn lines(&self) -> Vec<(Option<bool>, String, Option<usize>)> {
        let mut out = Vec::new();
        if let Some(e) = &self.error {
            out.push((None, e.clone(), None));
            return out;
        }
        if !self.listed {
            out.push((None, "Asking for fuzz targets…".into(), None));
            return out;
        }
        let workers = self.targets.iter().filter(|t| !t.core).count();
        out.push((None, format!("{} targets: {workers} workers, {} core surfaces. {}", self.targets.len(), self.targets.len() - workers,
            match (&self.running, self.on) {
                (Some(t), _) => format!("Fuzzing {t} ({} runs)", self.execs),
                (None, true) => "Between rounds".into(),
                (None, false) => "Stopped".into(),
            }), None));
        if let Some((t, m)) = &self.run_error {
            out.push((None, format!("{t} didn't run: {m}"), None));
        }
        if self.crashes.is_empty() {
            out.push((None, "No crashes found yet.".into(), None));
        }
        for (i, c) in self.crashes.iter().enumerate() {
            let at = c.frame.as_deref().map(|f| format!(" at {f}")).unwrap_or_default();
            let place = if c.in_core { "In the core (holds the app's grants)" } else { "In a worker (holds nothing)" };
            out.push((Some(c.in_core), format!("{place}: {}: {}{at}", c.target, c.summary), Some(i)));
            if let Some(fix) = &c.fix {
                out.push((None, format!("    → {fix}"), Some(i)));
            } else if c.in_core {
                out.push((None, "    → Move into a worker: parse it there and send the core a message".into(), Some(i)));
            }
        }
        out
    }
}

/// `cargo atrium worker add` (the SDK's "Move into a worker").
pub const WORKER_ADD: [&str; 4] = ["cargo", "atrium", "worker", "add"];

/// What to move for a crash at `frame` (`file:line` in the project,
/// whose source is `text`): the worker's name and the `--from` path of the
/// function the line is in.
pub fn move_target(frame: &str, text: &str) -> Result<(String, String), String> {
    let (file, line) = frame.rsplit_once(':').and_then(|(f, l)| Some((f, l.parse::<usize>().ok()?))).ok_or("no line to move")?;
    let module = file.strip_prefix("src/").and_then(|f| f.strip_suffix(".rs")).ok_or_else(|| format!("{file} isn't in src/"))?;
    if module == "main" || module.starts_with("bin/") {
        return Err(format!("{file} is the app's binary: move the function into the library first"));
    }
    let mut path: Vec<&str> = vec!["crate"];
    path.extend(module.split('/').filter(|m| *m != "lib" && *m != "mod"));
    let lines: Vec<&str> = text.lines().take(line).collect();
    let name = lines.iter().rev().find_map(|l| {
        let at = l.find("fn ")?;
        // `fn` as a word: start of line or after a space / `pub` etc.
        if at > 0 && !l[..at].ends_with(' ') && !l[..at].ends_with(')') {
            return None;
        }
        let rest = &l[at + 3..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        (!id.is_empty()).then_some(id)
    }).ok_or_else(|| format!("no function around {frame}"))?;
    path.push(&name);
    let mut worker: String = name.trim_matches('_').to_ascii_lowercase().replace('_', "-").chars().take(48).collect();
    if !worker.starts_with(|c: char| c.is_ascii_lowercase()) {
        worker.insert_str(0, "w-");
    }
    Ok((worker, path.join("::")))
}

/// The CLI's answer: what it changed (or would), the call for the core,
/// and the call sites to change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Move {
    pub worker: String,
    pub from: String,
    pub dry_run: bool,
    pub changed: Vec<String>,
    pub core_call: String,
    pub calls: Vec<String>,
    pub notes: Vec<String>,
}

pub fn read_move(out: &[u8], worker: &str, from: &str) -> Result<Move, String> {
    let text = String::from_utf8_lossy(out);
    // One JSON object, pretty or not, maybe after cargo's own lines.
    let v: Value = text.match_indices('{').find_map(|(i, _)| serde_json::from_str(text[i..].trim()).ok())
        .ok_or_else(|| format!("cargo atrium worker add: {}", text.trim().lines().last().unwrap_or("no answer")))?;
    if v["ok"] != Value::Bool(true) {
        return Err(v["error"].as_str().unwrap_or("failed").to_string());
    }
    let strs = |k: &str, f: &dyn Fn(&Value) -> String| v[k].as_array().map(|a| a.iter().map(f).collect()).unwrap_or_default();
    Ok(Move {
        worker: worker.into(),
        from: from.into(),
        dry_run: v["dry_run"].as_bool().unwrap_or(false),
        changed: strs("changed", &|c| format!("{} {}", c["action"].as_str().unwrap_or(""), c["path"].as_str().unwrap_or(""))),
        core_call: v["core_call"].as_str().unwrap_or("").into(),
        calls: strs("calls", &|c| format!("{}:{}: {}", c["path"].as_str().unwrap_or(""), c["line"], c["text"].as_str().unwrap_or("").trim())),
        notes: strs("notes", &|n| n.as_str().unwrap_or("").to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_core_crash_moves() {
        let src = "use x;\n\npub fn parse_header(b: &[u8]) -> u32 {\n    let n = b[4];\n    n as u32\n}\n";
        assert_eq!(move_target("src/lib.rs:4", src), Ok(("parse-header".into(), "crate::parse_header".into())));
        assert_eq!(move_target("src/fmt/bmp.rs:4", src).unwrap().1, "crate::fmt::bmp::parse_header");
        assert_eq!(move_target("src/fmt/mod.rs:4", src).unwrap().1, "crate::fmt::parse_header");
        assert!(move_target("src/main.rs:4", src).unwrap_err().contains("library first"));
        assert!(move_target("src/lib.rs:1", src).is_err());
        let m = read_move(br#"{"ok": true, "dry_run": true, "worker": "parse-header", "changed": [{"path": "atrium.toml", "action": "edited"}], "core_call": "demo::parse_header_worker::call", "calls": [{"path": "src/main.rs", "line": 63, "text": "  let h = parse_header(&b);"}], "notes": []}"#, "parse-header", "crate::parse_header").unwrap();
        assert_eq!(m.changed, vec!["edited atrium.toml"]);
        assert_eq!(m.calls, vec!["src/main.rs:63: let h = parse_header(&b);"]);
        assert!(m.dry_run);
        assert_eq!(read_move(br#"{"ok": false, "error": "generic"}"#, "a", "b"), Err("generic".into()));
    }

    #[test]
    fn targets_and_a_round() {
        let t = targets(br#"[{"target": "worker:decode", "kind": "worker", "about": "PNG bytes"}, {"target": "core:open-file", "kind": "core"}]"#).unwrap();
        assert_eq!(t.len(), 2);
        assert!(!t[0].core && t[1].core);
        assert!(targets(b"error: no such command: `atrium`").unwrap_err().contains("isn't in this workspace"));
        let mut f = Fuzz { targets: t, listed: true, ..Fuzz::default() };
        assert_eq!(f.next_target(), None);
        f.on = true;
        assert_eq!(f.next_target().as_deref(), Some("worker:decode"));
        assert_eq!(f.next_target().as_deref(), Some("core:open-file"));
        assert_eq!(f.next_target().as_deref(), Some("worker:decode"));
        f.running = Some("worker:decode".into());
        assert_eq!(f.next_target(), None);
        f.line("worker:decode", r#"{"event":"progress","execs":1200,"corpus":40,"secs":3}"#);
        assert_eq!(f.execs, 1200);
        f.line("worker:decode", r#"{"event":"crash","input":"fuzz/crashes/a","summary":"index out of bounds","frame":"src/decode.rs:40","in_core":false,"fix":null}"#);
        f.line("core:open-file", r#"{"event":"crash","input":"fuzz/crashes/b","summary":"overflow","frame":"src/main.rs:12","in_core":true,"fix":"Move into a worker (template: viewer)","binary":"target/fuzz/open-file"}"#);
        f.line("core:open-file", r#"{"event":"crash","input":"fuzz/crashes/c","summary":"overflow","frame":"src/main.rs:12","in_core":true}"#);
        f.line("x", "not json");
        f.line("worker:decode", r#"{"event":"error","message":"could not compile"}"#);
        assert!(f.lines().iter().any(|l| l.1 == "worker:decode didn't run: could not compile"));
        f.line("worker:decode", r#"{"event":"progress","execs":5}"#);
        assert!(f.run_error.is_none());
        assert_eq!(f.crashes.len(), 2);
        assert!(f.crashes[0].in_core, "core crashes first");
        let lines = f.lines();
        assert!(lines.iter().any(|l| l.0 == Some(true) && l.1.contains("In the core")));
        assert!(lines.iter().any(|l| l.1.contains("→ Move into a worker (template: viewer)")));
    }
}
