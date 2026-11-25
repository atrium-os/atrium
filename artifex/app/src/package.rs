//! The Package view (dev-tools.md §3.9): an Atrium app project made into a
//! signed bundle, tried on this computer, and described for a catalog.
//!
//! Every step runs the SDK's own tools in the workspace, the way the
//! person would in its terminal: `opifex token agent` names a P-256 key
//! the person keeps in Arca (the private half never leaves Arca), `cargo
//! atrium bundle --sign` (or `make bundle`) builds, packs and signs, and the
//! install sheet takes the `.insula` as a descriptor. Artifex holds no key
//! and parses only what those tools print.

use std::path::Path;

/// The key file `opifex token agent` writes, in the project (it names the
/// Arca key by its public half; it holds nothing secret).
pub const KEY_FILE: &str = ".atrium/publisher.key";
/// Its public key, the PEM a side-load list or a catalog takes.
pub const PUB_FILE: &str = ".atrium/publisher.pem";
/// The longest answer read back from a tool.
pub const OUT_MAX: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Builder {
    Cargo,
    Make,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Key {
    /// No key file in the project.
    #[default]
    None,
    /// `opifex token agent` is running.
    Making,
    /// The key file, and what it names ("Arca key \"name\"" or "a key file").
    Found(String),
    /// The last try failed: why, in the tool's words.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Build {
    #[default]
    Idle,
    Running,
    Done(Built),
    Failed(String),
}

/// What `cargo atrium bundle --json` made.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Built {
    /// The `.insula`, in `work` (as Artifex sees it).
    pub insula: String,
    /// The bundle's tree, in `work`.
    pub tree: String,
    pub arch: String,
    pub signed: bool,
    pub files: usize,
    pub bytes: u64,
    /// The content hash the signed `atrium.toml` names.
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Try {
    #[default]
    Idle,
    /// The install sheet is up.
    Asking,
    Installed,
    Declined,
    Failed(String),
}

/// The view's state for one project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Package {
    pub project: String,
    pub id: String,
    pub name: String,
    pub version: String,
    pub builder: Option<Builder>,
    pub key: Key,
    pub build: Build,
    pub tried: Try,
}

impl Package {
    /// The project in `dir` (as Artifex sees it), from its `atrium.toml`.
    pub fn read(project: &str, dir: &Path) -> Package {
        let text = std::fs::read_to_string(dir.join("atrium.toml")).unwrap_or_default();
        let v: toml::Value = text.parse().unwrap_or(toml::Value::Table(Default::default()));
        let s = |k: &str| v.get("app").and_then(|a| a.get(k)).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let builder = if dir.join("Cargo.toml").is_file() {
            Some(Builder::Cargo)
        } else if ["Makefile", "makefile", "BSDmakefile"].iter().any(|m| dir.join(m).is_file()) {
            Some(Builder::Make)
        } else {
            None
        };
        Package { project: project.into(), id: s("id"), name: s("name"), version: s("version"), builder, key: key_in(dir), ..Default::default() }
    }

    /// What stops a bundle from installing for real, in words; empty when
    /// nothing does.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.id.is_empty() {
            out.push("atrium.toml names no [app] id".into());
        } else if self.id.starts_with("dev.") {
            out.push(format!("{} is a dev. id: those run only from Artifex and never install; give it your own reverse-domain id", self.id));
        }
        if self.version.is_empty() {
            out.push("atrium.toml names no version".into());
        }
        if self.builder.is_none() {
            out.push("no Cargo.toml or Makefile to build it with".into());
        }
        out
    }

    pub fn ready(&self) -> bool {
        self.problems().is_empty() && matches!(self.key, Key::Found(_))
    }
}

/// The key file in `dir`, if there is one, and what it names.
pub fn key_in(dir: &Path) -> Key {
    match std::fs::read_to_string(dir.join(KEY_FILE)) {
        Ok(t) => Key::Found(key_label(&t)),
        Err(_) => Key::None,
    }
}

/// "Arca key \"comment\"" for an agent key file, else "a key file".
pub fn key_label(text: &str) -> String {
    if !text.lines().any(|l| l.replace(' ', "") == "atrium-agent-key=1") {
        return "a key file".into();
    }
    let public = text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "public").then(|| v.trim().trim_matches('"').to_string())
    });
    // "ecdsa-sha2-nistp256 <base64> <comment>"
    match public.as_deref().and_then(|p| p.splitn(3, ' ').nth(2)) {
        Some(c) if !c.is_empty() => format!("Arca key \"{}\"", c.chars().filter(|c| !c.is_control()).take(60).collect::<String>()),
        _ => "an Arca key".into(),
    }
}

/// Name the person's Arca publisher key in the project (`opifex token
/// agent`, which asks the workspace's SSH agent, so only a key the person
/// granted this workspace).
pub fn key_argv() -> Vec<String> {
    let script = format!("mkdir -p .atrium && exec opifex token agent --out {KEY_FILE} --pub {PUB_FILE}");
    vec!["sh".into(), "-c".into(), script]
}

/// Build, pack and sign (`--json`: the last thing printed is the result).
pub fn bundle_argv(b: &Builder) -> Vec<String> {
    match b {
        Builder::Cargo => ["cargo", "atrium", "bundle", "--sign", KEY_FILE, "--json"].iter().map(|s| s.to_string()).collect(),
        Builder::Make => vec!["make".into(), "bundle".into(), format!("BUNDLE_FLAGS=--sign {KEY_FILE} --json")],
    }
}

/// The last line of a tool's output that says something, for a failure.
pub fn last_words(out: &[u8]) -> String {
    let t = String::from_utf8_lossy(out);
    t.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it printed nothing").chars().take(200).collect()
}

/// `cargo atrium bundle --json`'s answer: the last top-level JSON object in
/// the output (the build's own lines come first).
pub fn read_bundle(out: &[u8]) -> Result<serde_json::Value, String> {
    let t = String::from_utf8_lossy(out);
    // A failure is one line; a success pretty-printed, from a bare "{" line.
    let last = t.lines().rev().map(str::trim).find(|l| !l.is_empty()).filter(|l| l.starts_with('{'))
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok());
    let start = t.rmatch_indices("\n{\n").next().map(|(i, _)| i + 1).or_else(|| t.starts_with("{\n").then_some(0));
    let v: serde_json::Value = last.or_else(|| start.and_then(|i| serde_json::from_str(t[i..].trim_end()).ok()))
        .ok_or_else(|| last_words(out))?;
    if v["ok"] != true {
        return Err(v["error"].as_str().map(str::to_string).unwrap_or_else(|| last_words(out)));
    }
    Ok(v)
}

/// `path` as the workspace's processes name it, in `work` (as Artifex sees
/// it): an absolute path relative to `remote_root`, a relative one to the
/// project the tool ran in. `None` for a path outside work or one that
/// climbs.
pub fn in_work(path: &str, remote_root: &str, project: &str) -> Option<String> {
    let joined;
    let rel = if path.starts_with('/') {
        path.strip_prefix(remote_root)?.trim_start_matches('/')
    } else if project.is_empty() {
        path
    } else {
        joined = format!("{project}/{path}");
        &joined
    };
    if rel.is_empty() || rel.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
        return None;
    }
    Some(rel.to_string())
}

/// The content hash a packed manifest names (`[bundle] content`).
pub fn content_of(manifest: &str) -> String {
    let v: toml::Value = manifest.parse().unwrap_or(toml::Value::Table(Default::default()));
    v.get("bundle").and_then(|b| b.get("content")).and_then(|c| c.as_str()).unwrap_or("").to_string()
}

/// The install sheet's answer to a side-load (opifex-sheet's `Answer`).
pub fn read_answer(line: &str) -> Try {
    match serde_json::from_str::<serde_json::Value>(line.trim_end()) {
        Ok(serde_json::Value::String(s)) if s == "Installed" => Try::Installed,
        Ok(serde_json::Value::String(s)) if s == "Declined" => Try::Declined,
        Ok(v) => match v.get("Failed").and_then(|f| f.get("why")).and_then(|w| w.as_str()) {
            Some(why) => Try::Failed(why.chars().filter(|c| !c.is_control()).take(300).collect()),
            None => Try::Failed("the install sheet answered something else".into()),
        },
        Err(_) => Try::Failed("the install sheet's answer was out of shape".into()),
    }
}

/// Where the install sheet listens (opifex-sheet's `SOCKET`), or its
/// stand-in (`OPIFEX_SHEET_SOCKET`).
pub fn sheet_socket() -> std::path::PathBuf {
    std::env::var_os("OPIFEX_SHEET_SOCKET").map(Into::into).unwrap_or_else(|| "/atrium/sockets/opifex-sheet/opifex-sheet.sock".into())
}

/// Ask the install sheet to install `file` for this person (it shows the
/// sheet; the person decides). Blocks until they do, up to 150 s: call it
/// off the UI thread.
pub fn side_load(sheet: &Path, file: &std::fs::File) -> Try {
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::AsRawFd;
    let s = match std::os::unix::net::UnixStream::connect(sheet) {
        Ok(s) => s,
        Err(e) => return Try::Failed(format!("no install sheet here ({e})")),
    };
    if s.set_read_timeout(Some(std::time::Duration::from_secs(150))).is_err() {
        return Try::Failed("the install sheet's socket".into());
    }
    if let Err(e) = atrium_fdpass::send_on(&s, b"\"SideLoad\"\n", &[file.as_raw_fd()]) {
        return Try::Failed(format!("the install sheet: {e}"));
    }
    // One line back, bounded.
    let mut line = String::new();
    match BufReader::new((&s).take(4096)).read_line(&mut line) {
        Ok(n) if n > 0 => read_answer(&line),
        Ok(_) => Try::Failed("the install sheet closed without an answer".into()),
        Err(e) => Try::Failed(format!("the install sheet: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_and_what_stops_it() {
        let d = std::env::temp_dir().join(format!("artifex-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".atrium")).unwrap();
        std::fs::write(d.join("atrium.toml"), "[app]\nid = \"dev.hello\"\nname = \"Hello\"\nversion = \"0.1.0\"\nentry = \"bin/hello\"\n").unwrap();
        std::fs::write(d.join("Makefile"), "PROG=hello\n").unwrap();
        let p = Package::read("hello", &d);
        assert_eq!((p.id.as_str(), p.version.as_str(), p.builder.clone()), ("dev.hello", "0.1.0", Some(Builder::Make)));
        assert_eq!(p.key, Key::None);
        assert_eq!(p.problems().len(), 1, "a dev. id never installs: {:?}", p.problems());
        assert!(!p.ready());
        std::fs::write(d.join("atrium.toml"), "[app]\nid = \"org.example.hello\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(d.join(KEY_FILE), "atrium-agent-key = 1\npublic = \"ecdsa-sha2-nistp256 AAAAE2Vj ana's apps\"\n").unwrap();
        let p = Package::read("hello", &d);
        assert!(p.problems().is_empty(), "{:?}", p.problems());
        assert_eq!(p.key, Key::Found("Arca key \"ana's apps\"".into()));
        assert!(p.ready());
        std::fs::write(d.join(KEY_FILE), "-----BEGIN PRIVATE KEY-----\n").unwrap();
        assert_eq!(key_in(&d), Key::Found("a key file".into()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_bundle_answer_after_the_build_lines() {
        let out = b"   Compiling hello v0.1.0\n    Finished release\n{\n  \"ok\": true,\n  \"id\": \"org.example.hello\",\n  \"arch\": \"amd64\",\n  \"tree\": \"/home/ana/work/hello/target/atrium-bundle/org.example.hello-0.1.0-amd64\",\n  \"insula\": \"/home/ana/work/hello/target/atrium-bundle/org.example.hello-0.1.0-amd64.insula\",\n  \"signed\": true,\n  \"files\": [\"bin/hello\"]\n}\n";
        let v = read_bundle(out).unwrap();
        assert_eq!(v["arch"], "amd64");
        assert_eq!(in_work(v["insula"].as_str().unwrap(), "/home/ana/work", "hello").as_deref(), Some("hello/target/atrium-bundle/org.example.hello-0.1.0-amd64.insula"));
        // A failure: the tool's last words.
        assert_eq!(read_bundle(b"error: could not compile `hello`\n").unwrap_err(), "error: could not compile `hello`");
        assert_eq!(read_bundle(b"x\n{\"ok\":false,\"error\":\"opifex pack failed\"}\n").unwrap_err(), "opifex pack failed");
        // Paths outside work, or climbing, aren't followed.
        assert_eq!(in_work("/etc/passwd", "/home/ana/work", "hello"), None);
        assert_eq!(in_work("/home/ana/work/../x.insula", "/home/ana/work", "hello"), None);
        // make bundle names it from the project: relative to it.
        assert_eq!(in_work("bundle/a-1-amd64.insula", "/home/ana/work", "app").as_deref(), Some("app/bundle/a-1-amd64.insula"));
        assert_eq!(in_work("bundle/a.insula", "/home/ana/work", "").as_deref(), Some("bundle/a.insula"));
        assert_eq!(in_work("../../etc/passwd", "/home/ana/work", "app"), None);
    }

    #[test]
    fn the_sheet_answers() {
        assert_eq!(read_answer("\"Installed\"\n"), Try::Installed);
        assert_eq!(read_answer("\"Declined\""), Try::Declined);
        assert_eq!(read_answer("{\"Failed\":{\"why\":\"not signed by a key on the side-load list\"}}"), Try::Failed("not signed by a key on the side-load list".into()));
        assert!(matches!(read_answer("{\"Added\":{\"name\":\"x\"}}"), Try::Failed(_)));
        assert!(matches!(read_answer("nonsense"), Try::Failed(_)));
    }

    #[test]
    fn side_load_sends_the_file_and_reads_one_line() {
        use std::io::{BufRead, BufReader, Write};
        let d = std::env::temp_dir().join(format!("artifex-sheet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let sock = d.join("sheet.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            use std::os::fd::AsRawFd;
            let mut buf = [0u8; 64];
            let r = atrium_fdpass::recv(s.as_raw_fd(), &mut buf, 1).unwrap();
            let got = String::from_utf8_lossy(&buf[..r.n]).to_string();
            let mut f = std::fs::File::from(r.fds.into_iter().next().unwrap());
            let mut body = String::new();
            std::io::Read::read_to_string(&mut f, &mut body).unwrap();
            (&s).write_all(b"\"Installed\"\n").unwrap();
            let _ = BufReader::new(&s).read_line(&mut String::new());
            (got, body)
        });
        let file = d.join("a.insula");
        std::fs::write(&file, "INSB").unwrap();
        let answer = side_load(&sock, &std::fs::File::open(&file).unwrap());
        assert_eq!(answer, Try::Installed);
        assert_eq!(t.join().unwrap(), ("\"SideLoad\"\n".to_string(), "INSB".to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn labels_and_argv() {
        assert_eq!(key_label("atrium-agent-key = 1\npublic = \"ecdsa-sha2-nistp256 AAAA\"\n"), "an Arca key");
        assert_eq!(bundle_argv(&Builder::Make)[2], format!("BUNDLE_FLAGS=--sign {KEY_FILE} --json"));
        assert_eq!(content_of("[app]\nid = \"x\"\n[bundle]\ncontent = \"sha256:ab\"\n"), "sha256:ab");
        assert_eq!(last_words(b"a\nb\n\n"), "b");
    }
}
