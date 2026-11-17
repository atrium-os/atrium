//! The composition panel (app-composition.md §6.1, plan M11): what the
//! project's `atrium.toml` makes of the app — the core and what it holds,
//! its workers (which hold nothing), its roles — with the consent the
//! person will see and `cargo atrium check`'s findings. Artifex reads the
//! manifest itself; the check runs in the workspace (`cargo atrium check
//! --json`, from the SDK's Rust layer).

use serde_json::Value;

/// The check, as Artifex runs it in the project folder.
pub const CHECK_ARGV: [&str; 4] = ["cargo", "atrium", "check", "--json"];
/// The templates `New project` offers.
pub const TEMPLATES_ARGV: [&str; 5] = ["cargo", "atrium", "new", "--list", "--json"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub name: String,
    pub entry: String,
    /// What it is given to work on (`fd`, `bytes`, …), as the manifest says.
    pub input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composition {
    /// The project folder in `work` the manifest is in.
    pub project: String,
    pub id: String,
    pub name: String,
    pub entry: String,
    /// The core's capabilities, as `key = value` lines.
    pub holds: Vec<String>,
    pub workers: Vec<Worker>,
    pub roles: Vec<String>,
    pub embeds: Vec<String>,
    pub consent: Vec<String>,
    pub findings: Vec<Finding>,
    /// The check has answered (or failed: `check_error`).
    pub checked: bool,
    pub check_error: Option<String>,
}

fn s(v: Option<&toml::Value>) -> String {
    v.and_then(toml::Value::as_str).unwrap_or("").to_string()
}

fn show(v: &toml::Value) -> String {
    match v {
        toml::Value::String(x) => format!("{x:?}"),
        toml::Value::Array(a) => format!("[{}]", a.iter().map(show).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// Read a manifest.
pub fn read(project: &str, text: &str) -> Result<Composition, String> {
    let v: toml::Value = text.parse().map_err(|e: toml::de::Error| format!("atrium.toml: {}", e.message()))?;
    let app = v.get("app");
    let mut c = Composition {
        project: project.to_string(),
        id: s(app.and_then(|a| a.get("id"))),
        name: s(app.and_then(|a| a.get("name"))),
        entry: s(app.and_then(|a| a.get("entry"))),
        ..Composition::default()
    };
    if let Some(caps) = v.get("capabilities").and_then(toml::Value::as_table) {
        for (k, val) in caps {
            if k == "embeds" {
                c.embeds = val.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
            }
            c.holds.push(format!("{k} = {}", show(val)));
        }
    }
    for w in v.get("workers").and_then(toml::Value::as_array).into_iter().flatten() {
        c.workers.push(Worker { name: s(w.get("name")), entry: s(w.get("entry")), input: s(w.get("input")) });
    }
    if let Some(roles) = v.get("roles").and_then(toml::Value::as_table) {
        c.roles = roles.keys().cloned().collect();
    }
    Ok(c)
}

impl Composition {
    /// Take `cargo atrium check --json`'s answer.
    pub fn take_check(&mut self, out: &[u8]) {
        self.checked = true;
        // The JSON is the last thing printed; cargo may print before it.
        let text = String::from_utf8_lossy(out);
        let start = text.find("{\n").or_else(|| text.find('{'));
        let v: Value = match start.map(|i| serde_json::from_str(&text[i..])) {
            Some(Ok(v)) => v,
            _ => {
                let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("no answer").to_string();
                self.check_error = Some(if first.contains("no such command") || first.contains("not found") {
                    "cargo atrium isn't in this workspace (it comes with the Rust toolchain layer)".into()
                } else {
                    first
                });
                return;
            }
        };
        self.check_error = None;
        self.consent = v.get("consent").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        self.findings = v.get("findings").and_then(Value::as_array).map(|a| a.iter().filter_map(|f| {
            Some(Finding {
                level: match f.get("level")?.as_str()? {
                    "error" => Level::Error,
                    "warning" => Level::Warning,
                    _ => Level::Note,
                },
                code: f.get("code").and_then(Value::as_str).unwrap_or("").to_string(),
                message: f.get("message")?.as_str()?.to_string(),
                fix: f.get("fix").and_then(Value::as_str).map(str::to_string),
            })
        }).collect()).unwrap_or_default();
        self.findings.sort_by_key(|f| f.level);
    }

    /// Lines for the panel, with a severity for colour (None: plain).
    pub fn lines(&self) -> Vec<(Option<Level>, String)> {
        let mut out = Vec::new();
        let title = if self.name.is_empty() { self.id.clone() } else { format!("{} ({})", self.name, self.id) };
        out.push((None, format!("Core: {title}, {}", if self.entry.is_empty() { "no entry" } else { &self.entry })));
        if self.holds.is_empty() {
            out.push((None, "    holds nothing".into()));
        }
        for h in &self.holds {
            out.push((None, format!("    holds {h}")));
        }
        if self.workers.is_empty() {
            out.push((None, "Workers: none (everything runs in the core)".into()));
        }
        for w in &self.workers {
            out.push((None, format!("Worker: {}, {}{}, holds nothing", w.name, w.entry,
                if w.input.is_empty() { String::new() } else { format!(", given {}", w.input) })));
        }
        if !self.roles.is_empty() {
            out.push((None, format!("Roles it serves: {}", self.roles.join(", "))));
        }
        if !self.embeds.is_empty() {
            out.push((None, format!("Embeds: {}", self.embeds.join(", "))));
        }
        if !self.consent.is_empty() {
            out.push((None, "At install the person is asked to allow:".into()));
            for c in &self.consent {
                out.push((None, format!("    {c}")));
            }
        }
        match (&self.check_error, self.checked) {
            (Some(e), _) => out.push((Some(Level::Note), format!("Check: {e}"))),
            (None, false) => out.push((None, "Checking…".into())),
            (None, true) if self.findings.is_empty() => out.push((None, "Check: nothing to fix".into())),
            _ => {}
        }
        for f in &self.findings {
            out.push((Some(f.level), f.message.clone()));
            if let Some(fix) = &f.fix {
                out.push((None, format!("    → {fix}")));
            }
        }
        out
    }
}

/// `cargo atrium new --list --json`: (name, about).
pub fn templates(out: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(out);
    let Some(i) = text.find('[') else { return Vec::new() };
    let v: Value = serde_json::from_str(&text[i..]).unwrap_or(Value::Null);
    v.as_array().map(|a| a.iter().filter_map(|t| Some((t.get("name")?.as_str()?.to_string(),
        t.get("about").and_then(Value::as_str).unwrap_or("").to_string()))).collect()).unwrap_or_default()
}

/// A name `New project` takes: a folder name cargo accepts as a package.
pub fn valid_project(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHOTOS: &str = r#"
[app]
id = "com.example.photos"
name = "Photos"
version = "1"
entry = "bin/photos"

[capabilities]
graphics = "fresco"
net-fetch = true
embeds = ["terminal"]

[[workers]]
name = "decode"
entry = "bin/photos-decode"
input = "fd"

[roles.viewer]
types = ["image/png"]
"#;

    #[test]
    fn reads_a_manifest_and_a_check() {
        let mut c = read("photos", PHOTOS).unwrap();
        assert_eq!((c.id.as_str(), c.name.as_str(), c.entry.as_str()), ("com.example.photos", "Photos", "bin/photos"));
        assert_eq!(c.workers, vec![Worker { name: "decode".into(), entry: "bin/photos-decode".into(), input: "fd".into() }]);
        assert!(c.holds.contains(&"net-fetch = true".to_string()));
        assert_eq!(c.embeds, vec!["terminal"]);
        assert_eq!(c.roles, vec!["viewer"]);
        c.take_check(br#"   Compiling x
{
  "manifest": "/home/p/work/photos/atrium.toml",
  "consent": ["Fetch from the web through Nuntius"],
  "findings": [
    {"level": "note", "code": "n", "message": "a note", "fix": null},
    {"level": "warning", "code": "parser-in-core", "message": "photos links png in its core", "fix": "decode in a worker (template: viewer)"}
  ]
}"#);
        assert!(c.checked && c.check_error.is_none());
        assert_eq!(c.consent, vec!["Fetch from the web through Nuntius"]);
        assert_eq!(c.findings[0].level, Level::Warning);
        assert_eq!(c.findings[0].fix.as_deref(), Some("decode in a worker (template: viewer)"));
        let lines = c.lines();
        assert!(lines.iter().any(|(_, l)| l.starts_with("Worker: decode")));
        assert!(lines.iter().any(|(_, l)| l.contains("→ decode in a worker")));
    }

    #[test]
    fn a_missing_cargo_atrium() {
        let mut c = read("x", "[app]\nid = \"a.b\"\n").unwrap();
        c.take_check(b"error: no such command: `atrium`\n");
        assert!(c.check_error.as_deref().unwrap().contains("isn't in this workspace"));
        assert!(read("x", "[app").is_err());
    }

    #[test]
    fn template_list_and_names() {
        let t = templates(br#"[{"name": "app", "about": "A window and a core"}]"#);
        assert_eq!(t, vec![("app".to_string(), "A window and a core".to_string())]);
        assert!(templates(b"garbage").is_empty());
        assert!(valid_project("my-app_2"));
        assert!(!valid_project("My"));
        assert!(!valid_project("../x"));
        assert!(!valid_project(""));
    }
}
