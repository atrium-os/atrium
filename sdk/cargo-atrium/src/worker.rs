//! `cargo atrium worker add` (app-composition.md §4, §6): move a part of
//! an app into a `[[workers]]` entry of its own, the engine under
//! Artifex's "Move into a worker".
//!
//! It writes, in the project folder:
//!
//! - atrium.toml: a `[[workers]]` entry (`input = "fd"` or `"message"`);
//! - Cargo.toml: a `[[bin]]` for the worker, and the atrium-message and
//!   atrium-worker dependencies when they are missing;
//! - `src/<name>_worker.rs` (a module of the library): the messages, the
//!   worker's handler and `call`, the core's side of it;
//! - `src/bin/<name>.rs`: `atrium_worker::main!(<lib>::workers::<name>)`;
//! - the library's `workers` module: `pub use` of the handler, where
//!   `cargo atrium fuzz` finds it.
//!
//! With `--from <path::fn>` the handler runs that library function, and
//! `call` takes the function's own arguments and gives back what it gave,
//! as a `Result<_, String>`: the core's calls change from `f(x)` to
//! `<name>_worker::call(x)`. It doesn't rewrite them; it lists where they
//! are. Only library code can move (a worker is another binary): a
//! function in `src/main.rs` has to go into the library first.
//!
//! Every edit is an insertion or an appended block, so comments and
//! layout stay as they were.

use std::path::{Path, PathBuf};

/// What the worker's input is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// An open file (`input = "fd"`): the core never reads it.
    Fd,
    /// Bytes the core holds, in a message (`input = "message"`).
    Bytes,
    /// A message with fields (`input = "message"`).
    Message,
}

impl Input {
    pub fn parse(s: &str) -> Option<Input> {
        match s {
            "fd" => Some(Input::Fd),
            "bytes" => Some(Input::Bytes),
            "message" => Some(Input::Message),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Input::Fd => "fd",
            Input::Bytes => "bytes",
            Input::Message => "message",
        }
    }
    fn manifest(self) -> &'static str {
        if self == Input::Fd { "fd" } else { "message" }
    }
}

pub struct AddOpts<'a> {
    pub name: &'a str,
    pub input: Input,
    pub from: Option<&'a str>,
    /// "call" (a fresh worker each call) or "session".
    pub lifetime: &'a str,
}

/// One file the plan writes.
#[derive(Debug)]
pub struct Write {
    pub path: String,
    pub created: bool,
    pub text: String,
}

/// A place in the project that calls the moved function.
#[derive(Debug, PartialEq)]
pub struct CallSite {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug)]
pub struct Plan {
    pub writes: Vec<Write>,
    /// The core's side: `<lib>::<name>_worker::call`.
    pub core_call: String,
    /// The handler, as `cargo atrium fuzz` names it: `worker:<name>`.
    pub fuzz_target: String,
    pub calls: Vec<CallSite>,
    pub notes: Vec<String>,
}

/// Bounds for the generated fields.
const BYTES_MAX: u64 = 16 << 20;
const TEXT_MAX: u64 = 65_536;
const ANSWER_MAX: u64 = 64 << 20;
const FILE_MAX: u64 = 64 << 20;

/// Work out the edits; nothing is written.
pub fn plan(project: &Path, o: &AddOpts) -> Result<Plan, String> {
    if !crate::new::valid_name(o.name) {
        return Err(format!("{:?}: a worker name is lowercase letters, digits and dashes", o.name));
    }
    if o.lifetime != "call" && o.lifetime != "session" {
        return Err(format!("--lifetime {:?}: call or session", o.lifetime));
    }
    let snake = o.name.replace('-', "_");
    let pascal = pascal(o.name);
    let read = |rel: &str| std::fs::read_to_string(project.join(rel)).map_err(|e| format!("{rel}: {e}"));
    let cargo = read("Cargo.toml")?;
    let atrium = read("atrium.toml")?;
    let cv: toml::Value = cargo.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
    let av: toml::Value = atrium.parse().map_err(|e| format!("atrium.toml: {e}"))?;
    let package = cv.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str())
        .ok_or("Cargo.toml: no [package] name")?.to_string();
    let lib = crate::fuzz::lib_name_of(&cv).ok_or("Cargo.toml: no package name")?;
    if av.get("workers").and_then(|w| w.as_array()).into_iter().flatten()
        .any(|w| w.get("name").and_then(|n| n.as_str()) == Some(o.name)) {
        return Err(format!("atrium.toml already has a worker {:?}", o.name));
    }
    let lib_path = cv.get("lib").and_then(|l| l.get("path")).and_then(|p| p.as_str()).unwrap_or("src/lib.rs").to_string();
    let lib_dir = Path::new(&lib_path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "src".into());
    let module = format!("{snake}_worker");
    let module_path = format!("{lib_dir}/{module}.rs");
    let bin_path = format!("src/bin/{snake}.rs");
    let bin_name = format!("{package}-{}", o.name);
    for p in [&module_path, &bin_path] {
        if project.join(p).exists() {
            return Err(format!("{p} exists already"));
        }
    }
    if cv.get("bin").and_then(|b| b.as_array()).into_iter().flatten()
        .any(|b| b.get("name").and_then(|n| n.as_str()) == Some(bin_name.as_str())) {
        return Err(format!("Cargo.toml already has a [[bin]] {bin_name:?}"));
    }

    let mut notes = Vec::new();
    let sources = rust_sources(project);
    let lib_exists = project.join(&lib_path).exists();

    // What moves, and its signature.
    let moved = match o.from {
        Some(path) => {
            let (f, sig) = find_fn(project, &sources, path, &lib, &lib_path)?;
            Some((f, sig))
        }
        None => None,
    };
    let code = match &moved {
        Some((fpath, sig)) => from_fn(o, &pascal, fpath, sig, &sources, project, &mut notes)?,
        None => placeholder(o, &pascal),
    };

    let mut writes = Vec::new();

    // atrium.toml: a [[workers]] entry at the end.
    let mut a = atrium.clone();
    if !a.ends_with('\n') {
        a.push('\n');
    }
    a.push_str(&format!(
        "\n# Made by `cargo atrium worker add`: no capabilities, a jail of its own{}.\n\
         [[workers]]\nname = \"{}\"\nentry = \"bin/{bin_name}\"\ninput = \"{}\"\nlifetime = \"{}\"\nmemory = \"256M\"\ncpu = \"10s\"\n",
        if o.lifetime == "call" { " for each call" } else { " for each session" },
        o.name, o.input.manifest(), o.lifetime,
    ));
    writes.push(Write { path: "atrium.toml".into(), created: false, text: a });

    // Cargo.toml: the dependencies, then a [[bin]].
    let mut c = cargo.clone();
    let deps = cv.get("dependencies").and_then(|d| d.as_table());
    let missing: Vec<&str> = ["atrium-message", "atrium-worker"].into_iter()
        .filter(|d| deps.map(|t| !t.contains_key(*d)).unwrap_or(true)).collect();
    if !missing.is_empty() {
        let lines: String = missing.iter().map(|d| format!("{d} = {}\n", sdk_dep(&cv, d))).collect();
        c = insert_after_header(&c, "[dependencies]", &lines);
    }
    if !c.ends_with('\n') {
        c.push('\n');
    }
    c.push_str(&format!("\n# The {} worker: no capabilities (atrium.toml's [[workers]]).\n[[bin]]\nname = \"{bin_name}\"\npath = \"{bin_path}\"\n", o.name));
    writes.push(Write { path: "Cargo.toml".into(), created: false, text: c });

    // The library: the module, and the workers module's re-export.
    let reexport = format!("pub use crate::{module}::{snake};");
    let mut l = if lib_exists { read(&lib_path)? } else {
        format!("//! What {package}'s core and its workers share.\n")
    };
    if !l.ends_with('\n') {
        l.push('\n');
    }
    l.push_str(&format!("\n/// The {} worker: its messages, its handler and the core's `call`.\npub mod {module};\n", o.name));
    let workers_file = ["workers.rs", "workers/mod.rs"].iter().map(|f| format!("{lib_dir}/{f}")).find(|f| project.join(f).exists());
    if let Some(at) = find_line(&l, |t| t.starts_with("pub mod workers") && t.ends_with('{')) {
        let mut end = l[at..].find('\n').map(|n| at + n + 1).unwrap_or(l.len());
        // After the re-exports already there, so they stay in order.
        while l[end..].trim_start_matches([' ', '\t']).starts_with("pub use crate::") {
            end = l[end..].find('\n').map(|n| end + n + 1).unwrap_or(l.len());
        }
        l.insert_str(end, &format!("    {reexport}\n"));
    } else if find_line(&l, |t| t == "pub mod workers;").is_some() && workers_file.is_some() {
        let wf = workers_file.unwrap();
        let mut w = read(&wf)?;
        if !w.ends_with('\n') {
            w.push('\n');
        }
        w.push_str(&format!("{reexport}\n"));
        writes.push(Write { path: wf, created: false, text: w });
    } else {
        l.push_str(&format!(
            "\n/// The workers' handlers: `cargo atrium fuzz` finds `workers::<name>` for\n/// each `[[workers]]` entry.\npub mod workers {{\n    {reexport}\n}}\n"
        ));
    }
    writes.push(Write { path: lib_path.clone(), created: !lib_exists, text: l });

    let module_text = format!(
        "//! The {name} worker (atrium.toml's `[[workers]]` \"{name}\"), made by\n\
         //! `cargo atrium worker add`. [`{snake}`] runs in the worker, with no\n\
         //! capabilities; the core calls [`call`]. `cargo atrium fuzz worker:{name}`\n\
         //! fuzzes the handler.\n\n\
         use atrium_message::Message;\n\
         #[allow(unused_imports)]\n\
         use std::fs::File;\n\n{code}",
        name = o.name,
    );
    writes.push(Write { path: module_path.clone(), created: true, text: module_text });
    writes.push(Write {
        path: bin_path.clone(),
        created: true,
        text: format!(
            "//! The {} worker (atrium.toml's `[[workers]]`): no capabilities; it\n\
             //! answers the core's calls with [`{lib}::{module}`]'s handler, in\n\
             //! capability mode: it opens nothing by name, only what it is given.\n\n\
             atrium_worker::main!({lib}::workers::{snake});\n",
            o.name
        ),
    });

    // Where the core calls what moved.
    let mut calls = Vec::new();
    if let Some((fpath, _)) = &moved {
        let fname = fpath.rsplit("::").next().unwrap_or(fpath);
        for (rel, text) in &sources {
            for (i, line) in text.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") || is_definition(t, fname) {
                    continue;
                }
                if calls_fn(line, fname) {
                    calls.push(CallSite { path: rel.clone(), line: i + 1, text: line.trim().to_string() });
                }
            }
        }
        if !calls.is_empty() {
            notes.push(format!("replace the calls listed with {lib}::{module}::call(...) in the core (it returns Result<_, String>)"));
        }
    }
    if o.lifetime == "session" {
        notes.push(format!("lifetime = \"session\": for many calls to one worker, hold Workers::get()?.session(\"{}\") in the core instead of call()", o.name));
    }
    Ok(Plan {
        writes,
        core_call: format!("{lib}::{module}::call"),
        fuzz_target: format!("worker:{}", o.name),
        calls,
        notes,
    })
}

/// Write the plan's files.
pub fn apply(project: &Path, p: &Plan) -> Result<(), String> {
    for w in &p.writes {
        let f = project.join(&w.path);
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(&f, &w.text).map_err(|e| format!("{}: {e}", w.path))?;
    }
    Ok(())
}

fn pascal(name: &str) -> String {
    name.split('-').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
    }).collect()
}

fn find_line(text: &str, f: impl Fn(&str) -> bool) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if f(line.trim()) {
            return Some(at);
        }
        at += line.len();
    }
    None
}

fn insert_after_header(text: &str, header: &str, lines: &str) -> String {
    match find_line(text, |t| t == header) {
        Some(at) => {
            let end = text[at..].find('\n').map(|n| at + n + 1).unwrap_or(text.len());
            let mut s = text.to_string();
            if end == s.len() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.insert_str(end.min(s.len()), lines);
            s
        }
        None => {
            let mut s = text.to_string();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&format!("\n{header}\n{lines}"));
            s
        }
    }
}

/// An SDK dependency written like the project's others: a path next to
/// pergola's, or the registry.
fn sdk_dep(cv: &toml::Value, krate: &str) -> String {
    let deps = cv.get("dependencies").and_then(|d| d.as_table());
    let path = deps.and_then(|t| ["pergola", "libatrium", "atrium-open", "limen", "nuntius", "fresco-client"].iter()
        .find_map(|k| t.get(*k).and_then(|d| d.get("path")).and_then(|p| p.as_str()).map(PathBuf::from)));
    match path.and_then(|p| p.parent().map(Path::to_path_buf)) {
        Some(root) => format!("{{ path = {:?} }}", root.join("sdk").join(krate).display().to_string()),
        None => format!("{{ version = \"{}\", registry = \"atrium\" }}", crate::new::SDK_VERSION),
    }
}

/// The project's Rust files (relative path, text), but not target/ or
/// anything hidden.
fn rust_sources(project: &Path) -> Vec<(String, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(base, &p, out);
            } else if ft.is_file() && name.ends_with(".rs") {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    out.push((p.strip_prefix(base).unwrap_or(&p).to_string_lossy().into_owned(), t));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(project, &project.join("src"), &mut out);
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_definition(line: &str, fname: &str) -> bool {
    let mut s = line;
    for p in ["pub(crate) ", "pub(super) ", "pub ", "const ", "async ", "unsafe ", "extern \"C\" "] {
        s = s.strip_prefix(p).unwrap_or(s);
    }
    s.strip_prefix("fn ").map(|r| r.starts_with(fname) && !r[fname.len()..].starts_with(is_ident_char)).unwrap_or(false)
}

/// `fname(` or `fname::<` as a whole word.
fn calls_fn(line: &str, fname: &str) -> bool {
    let mut from = 0;
    while let Some(i) = line[from..].find(fname) {
        let at = from + i;
        let before = line[..at].chars().next_back();
        let after = line[at + fname.len()..].trim_start();
        if !before.map(is_ident_char).unwrap_or(false) && (after.starts_with('(') || after.starts_with("::<")) {
            return true;
        }
        from = at + fname.len();
    }
    false
}

/// A function's signature, as written.
#[derive(Debug, PartialEq)]
pub struct Sig {
    pub params: Vec<(String, String)>,
    /// What it returns; "()" for nothing.
    pub ret: String,
}

/// Find `path`'s function in the library and read its signature. Gives
/// back the path to call it by (`crate::...`).
fn find_fn(project: &Path, sources: &[(String, String)], path: &str, lib: &str, lib_path: &str) -> Result<(String, Sig), String> {
    let _ = project;
    let rel = path.strip_prefix("crate::").or_else(|| path.strip_prefix(&format!("{lib}::"))).unwrap_or(path);
    if rel.is_empty() || !rel.split("::").all(|s| !s.is_empty() && s.chars().all(is_ident_char)) {
        return Err(format!("--from {path:?}: a path to a function, like parse::feed"));
    }
    let fname = rel.rsplit("::").next().unwrap_or(rel);
    let mods: Vec<&str> = rel.split("::").collect();
    let mods = &mods[..mods.len() - 1];
    let lib_dir = Path::new(lib_path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "src".into());
    // The files the module path can be in: its own file, or any file of
    // the library (inline modules) when that file isn't there.
    let mut candidates: Vec<String> = Vec::new();
    if mods.is_empty() {
        candidates.push(lib_path.to_string());
    } else {
        let m = mods.join("/");
        candidates.push(format!("{lib_dir}/{m}.rs"));
        candidates.push(format!("{lib_dir}/{m}/mod.rs"));
    }
    let in_lib = |rel: &str| !rel.starts_with("src/bin/") && rel != "src/main.rs";
    let mut found: Vec<(&str, usize)> = Vec::new();
    for (rel, text) in sources {
        if !in_lib(rel) {
            continue;
        }
        if let Some(at) = def_at(text, fname) {
            found.push((rel.as_str(), at));
        }
    }
    let pick = found.iter().find(|(r, _)| candidates.iter().any(|c| c == r)).or_else(|| found.first());
    let Some((rel_file, at)) = pick else {
        let elsewhere = sources.iter().find(|(r, t)| !in_lib(r) && def_at(t, fname).is_some());
        return Err(match elsewhere {
            Some((r, _)) => format!(
                "{fname} is in {r}, a binary: a worker runs library code, so move it into the library ({lib_path}) first"
            ),
            None => format!("no fn {fname} in the library ({lib_path} and its modules)"),
        });
    };
    let text = &sources.iter().find(|(r, _)| r == rel_file).unwrap().1;
    let sig = parse_sig(&text[*at..]).map_err(|e| format!("{fname} ({rel_file}): {e}"))?;
    Ok((format!("crate::{rel}"), sig))
}

/// Where `fn fname` is defined in `text` (the offset of `fn`).
fn def_at(text: &str, fname: &str) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if !t.starts_with("//") && is_definition(t.trim_end(), fname) {
            let fn_at = line.find("fn ")?;
            return Some(at + fn_at);
        }
        at += line.len();
    }
    None
}

/// Split at commas not inside <>, (), [].
fn split_top(s: &str) -> Vec<String> {
    let (mut depth, mut cur, mut out) = (0i32, String::new(), Vec::new());
    let mut prev = ' ';
    for c in s.chars() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' if prev != '-' => depth -= 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                prev = c;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        prev = c;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Strip lifetimes (`&'a str` is `&str`) and squeeze spaces.
fn norm_ty(t: &str) -> String {
    let mut s = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            while chars.peek().map(|c| is_ident_char(*c)).unwrap_or(false) {
                chars.next();
            }
            while chars.peek() == Some(&' ') {
                chars.next();
            }
            continue;
        }
        s.push(c);
    }
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.replace("& ", "&").replace("< ", "<").replace(" >", ">").replace("std::string::String", "String")
}

/// `fn name(params) -> Ret {` from the start of `fn`.
pub fn parse_sig(s: &str) -> Result<Sig, String> {
    let open = s.find(['(', '<']).ok_or("no parameter list")?;
    if s.as_bytes()[open] == b'<' {
        return Err("a generic function can't move as is: give the worker concrete types".into());
    }
    let mut depth = 0;
    let mut close = None;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.ok_or("unclosed parameter list")?;
    let mut params = Vec::new();
    for (i, p) in split_top(&s[open + 1..close]).into_iter().enumerate() {
        if p.contains("self") && !p.contains(':') {
            return Err("a method can't move as is: a worker calls a free function".into());
        }
        let (pat, ty) = p.split_once(':').ok_or_else(|| format!("parameter {p:?} has no type"))?;
        let pat = pat.trim().trim_start_matches("mut ").trim();
        let name = if !pat.is_empty() && pat.chars().all(is_ident_char) && pat != "_" && !pat.starts_with('_') {
            pat.to_string()
        } else {
            format!("arg{i}")
        };
        let ty = norm_ty(ty);
        if ty.starts_with("impl ") || ty.starts_with("&mut") && !is_file(&ty) || ty.starts_with("dyn ") || ty.starts_with("&dyn") {
            return Err(format!("parameter {name}: {ty} can't cross to a worker; take an owned or & value"));
        }
        params.push((name, ty));
    }
    let rest = &s[close + 1..];
    let end = rest.find(['{', ';']).unwrap_or(rest.len());
    let mut head = rest[..end].trim();
    if let Some(w) = head.find("where") {
        head = head[..w].trim();
    }
    let ret = head.strip_prefix("->").map(|r| norm_ty(r.trim())).unwrap_or_else(|| "()".into());
    Ok(Sig { params, ret })
}

fn is_file(ty: &str) -> bool {
    matches!(ty.trim_start_matches('&').trim_start_matches("mut ").trim(), "File" | "std::fs::File" | "fs::File")
}

/// The type a field holds for a parameter of type `ty`.
fn owned(ty: &str) -> String {
    if let Some(w) = sized(ty.trim_start_matches('&')) {
        w.into()
    } else if ty == "&str" {
        "String".into()
    } else if let Some(inner) = ty.strip_prefix("&[").and_then(|t| t.strip_suffix(']')) {
        format!("Vec<{inner}>")
    } else if let Some(t) = ty.strip_prefix('&') {
        t.to_string()
    } else {
        ty.to_string()
    }
}

/// From a parameter to the field (the core's side).
fn to_field(name: &str, ty: &str) -> String {
    if let Some(w) = sized(ty.trim_start_matches('&')) {
        format!("{}{name} as {w}", if ty.starts_with('&') { "*" } else { "" })
    } else if ty == "&str" {
        format!("{name}.to_string()")
    } else if ty.starts_with("&[") {
        format!("{name}.to_vec()")
    } else if ty.starts_with('&') {
        format!("{name}.clone()")
    } else {
        name.to_string()
    }
}

/// From the field to the argument (the worker's side).
fn to_arg(name: &str, ty: &str) -> String {
    if let Some(_) = sized(ty.trim_start_matches('&')) {
        format!("{}({name} as {})", if ty.starts_with('&') { "&" } else { "" }, ty.trim_start_matches('&'))
    } else if ty.starts_with('&') { format!("&{name}") } else { name.to_string() }
}

/// usize and isize cross as u64 and i64.
fn sized(ty: &str) -> Option<&'static str> {
    match ty {
        "usize" => Some("u64"),
        "isize" => Some("i64"),
        _ => None,
    }
}

/// Types the wire carries without a derive of the project's.
fn is_wire(ty: &str) -> bool {
    let prims = ["u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32", "f64", "bool", "String"];
    if prims.contains(&ty) {
        return true;
    }
    for wrap in ["Vec<", "Option<", "Box<"] {
        if let Some(inner) = ty.strip_prefix(wrap).and_then(|t| t.strip_suffix('>')) {
            return is_wire(inner);
        }
    }
    false
}

fn bound_attr(ty: &str, max: u64) -> String {
    if ty == "String" || ty.starts_with("Vec<") {
        format!("    #[message(max_len = {max})]\n")
    } else {
        String::new()
    }
}

/// `Result<T, E>` → (T, Some(E)); else (ret, None).
fn split_result(ret: &str) -> (String, Option<String>) {
    for p in ["Result<", "std::result::Result<", "io::Result<", "std::io::Result<"] {
        if let Some(inner) = ret.strip_prefix(p).and_then(|t| t.strip_suffix('>')) {
            let parts = split_top(inner);
            let e = parts.get(1).cloned().unwrap_or_else(|| "std::io::Error".into());
            return (parts[0].clone(), Some(e));
        }
    }
    (ret.to_string(), None)
}

const DERIVE: &str = "#[derive(Message, Debug, Clone, PartialEq)]\n#[message(version = 1)]\n";
const DERIVE_DEFAULT: &str = "#[derive(Message, Debug, Clone, Default, PartialEq)]\n#[message(version = 1)]\n";

fn call_fn(name: &str, params: &str, ask: &str, ret: &str, map: &str, file: &str) -> String {
    format!(
        "/// The core's side: runs this in the `{name}` worker (a jail with no\n\
         /// capabilities) and gives back its answer, checked as it arrived.\n\
         pub fn call({params}) -> Result<{ret}, String> {{\n\
         \x20   let workers = atrium_worker::Workers::get().map_err(|e| e.to_string())?;\n\
         \x20   workers.call(\"{name}\", &{ask}, {file}).map_err(|e| e.to_string()){map}\n\
         }}\n"
    )
}

const READ_FILE: &str = "    let file = file.ok_or(\"no file\")?;\n    let mut bytes = Vec::new();\n    std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_FILE + 1), &mut bytes).map_err(|e| e.to_string())?;\n    if bytes.len() as u64 > MAX_FILE {\n        return Err(format!(\"the file is larger than {MAX_FILE} bytes\"));\n    }\n";

/// The code without `--from`: a handler that measures its input, for the
/// person to replace.
fn placeholder(o: &AddOpts, p: &str) -> String {
    let snake = o.name.replace('-', "_");
    let name = o.name;
    match o.input {
        Input::Fd => format!(
            "/// The largest file the worker reads.\npub const MAX_FILE: u64 = {FILE_MAX};\n\n\
             /// What the core asks (the file rides beside it).\n{DERIVE_DEFAULT}pub struct {p}Ask {{}}\n\n\
             /// What the worker answers.\n{DERIVE}pub struct {p}Answer {{\n    pub size: u64,\n}}\n\n\
             /// The worker: the file the core passed, unread by the core. Replace\n\
             /// the body with your parsing; any crate is fine here.\n\
             pub fn {snake}(_ask: {p}Ask, file: Option<File>) -> Result<{p}Answer, String> {{\n{READ_FILE}    Ok({p}Answer {{ size: bytes.len() as u64 }})\n}}\n\n{}",
            call_fn(name, "file: File", &format!("{p}Ask {{}}"), &format!("{p}Answer"), "", "Some(file)")
        ),
        Input::Bytes => format!(
            "/// What the core asks: bytes it holds and doesn't trust.\n{DERIVE_DEFAULT}pub struct {p}Ask {{\n    #[message(max_len = {BYTES_MAX})]\n    pub bytes: Vec<u8>,\n}}\n\n\
             /// What the worker answers.\n{DERIVE}pub struct {p}Answer {{\n    pub size: u64,\n}}\n\n\
             /// The worker. Replace the body with your parsing; any crate is fine here.\n\
             pub fn {snake}(ask: {p}Ask, _file: Option<File>) -> Result<{p}Answer, String> {{\n    Ok({p}Answer {{ size: ask.bytes.len() as u64 }})\n}}\n\n{}",
            call_fn(name, "bytes: &[u8]", &format!("{p}Ask {{ bytes: bytes.to_vec() }}"), &format!("{p}Answer"), "", "None")
        ),
        Input::Message => format!(
            "/// What the core asks.\n{DERIVE_DEFAULT}pub struct {p}Ask {{\n    #[message(max_len = {TEXT_MAX})]\n    pub text: String,\n}}\n\n\
             /// What the worker answers.\n{DERIVE}pub struct {p}Answer {{\n    pub words: u64,\n}}\n\n\
             /// The worker. Replace the fields and the body with yours.\n\
             pub fn {snake}(ask: {p}Ask, _file: Option<File>) -> Result<{p}Answer, String> {{\n    Ok({p}Answer {{ words: ask.text.split_whitespace().count() as u64 }})\n}}\n\n{}",
            call_fn(name, "text: &str", &format!("{p}Ask {{ text: text.to_string() }}"), &format!("{p}Answer"), "", "None")
        ),
    }
}

/// Whether the project's sources derive Message for `ty`.
fn derives_message(sources: &[(String, String)], ty: &str) -> bool {
    let base = ty.rsplit("::").next().unwrap_or(ty);
    sources.iter().any(|(_, t)| {
        let lines: Vec<&str> = t.lines().collect();
        lines.iter().enumerate().any(|(i, l)| {
            let l = l.trim_start().trim_start_matches("pub ");
            let is_def = ["struct ", "enum "].iter().any(|k| {
                l.strip_prefix(k).map(|r| r.starts_with(base) && !r[base.len()..].starts_with(is_ident_char)).unwrap_or(false)
            });
            is_def && lines[i.saturating_sub(4)..i].iter().any(|d| d.contains("derive(") && d.contains("Message"))
        })
    })
}

/// The code with `--from`: the handler calls the function; `call` takes
/// its arguments.
fn from_fn(o: &AddOpts, p: &str, fpath: &str, sig: &Sig, sources: &[(String, String)], _project: &Path, notes: &mut Vec<String>) -> Result<String, String> {
    let snake = o.name.replace('-', "_");
    let name = o.name;
    let (t, err) = split_result(&sig.ret);
    let (wt, to_wire, back) = match sized(&t) {
        Some(w) => (w.to_string(), format!(" as {w}"), format!(" as {t}")),
        None => (t.clone(), String::new(), String::new()),
    };
    let wrap = is_wire(&wt) || t == "()";
    let answer_def = if t == "()" {
        format!("/// What the worker answers: that it ran.\n{DERIVE}pub struct {p}Answer {{}}\n")
    } else if wrap {
        format!("/// What the worker answers: what {fpath} gave back.\n{DERIVE}pub struct {p}Answer {{\n{}    pub value: {wt},\n}}\n", bound_attr(&wt, ANSWER_MAX))
    } else {
        if !derives_message(sources, &t) {
            notes.push(format!("{t} is the answer: give it #[derive(Message)] and #[message(version = 1)] (atrium_message)"));
        }
        format!("/// What the worker answers: {fpath}'s own type.\npub type {p}Answer = {t};\n")
    };
    let run = |args: &str| match &err {
        Some(_) => format!("{fpath}({args}).map_err(|e| e.to_string())?"),
        None => format!("{fpath}({args})"),
    };
    let answer = |v: &str| {
        if t == "()" {
            format!("    {v};\n    Ok({p}Answer {{}})\n")
        } else if wrap {
            format!("    Ok({p}Answer {{ value: {v}{to_wire} }})\n")
        } else {
            format!("    Ok({v})\n")
        }
    };
    let (ret, map) = if t == "()" {
        ("()".to_string(), ".map(|_: ".to_string() + p + "Answer| ())")
    } else if wrap {
        (t.clone(), format!(".map(|a: {p}Answer| a.value{back})"))
    } else {
        (t.clone(), String::new())
    };
    match o.input {
        Input::Fd => {
            let [(pname, pty)] = sig.params.as_slice() else {
                return Err(format!("with --input fd, {fpath} takes the file and nothing else; it takes {} parameters (use --input message)", sig.params.len()));
            };
            let arg = if is_file(pty) {
                if pty.starts_with('&') { "&file".to_string() } else { "file".to_string() }
            } else {
                match pty.as_str() {
                    "&[u8]" => "&bytes".into(),
                    "Vec<u8>" => "bytes".into(),
                    "&str" => "std::str::from_utf8(&bytes).map_err(|e| e.to_string())?".into(),
                    "String" => "String::from_utf8(bytes).map_err(|e| e.to_string())?".into(),
                    other => return Err(format!("with --input fd, {fpath}'s {pname} is a file's contents: &[u8], Vec<u8>, &str, String or File, not {other}")),
                }
            };
            let read = if is_file(pty) { "    let file = file.ok_or(\"no file\")?;\n".to_string() } else { READ_FILE.to_string() };
            let max = if is_file(pty) { String::new() } else { format!("/// The largest file the worker reads.\npub const MAX_FILE: u64 = {FILE_MAX};\n\n") };
            Ok(format!(
                "{max}/// What the core asks (the file rides beside it).\n{DERIVE_DEFAULT}pub struct {p}Ask {{}}\n\n{answer_def}\n\
                 /// The worker: {fpath} on the file the core passed, unread by the core.\n\
                 pub fn {snake}(_ask: {p}Ask, file: Option<File>) -> Result<{p}Answer, String> {{\n{read}{}}}\n\n{}",
                answer(&run(&arg)),
                call_fn(name, "file: File", &format!("{p}Ask {{}}"), &ret, &map, "Some(file)")
            ))
        }
        Input::Bytes | Input::Message => {
            if o.input == Input::Bytes && !matches!(sig.params.as_slice(), [(_, t)] if matches!(t.as_str(), "&[u8]" | "Vec<u8>" | "&str" | "String")) {
                return Err(format!("with --input bytes, {fpath} takes one &[u8], Vec<u8>, &str or String (use --input message for other parameters)"));
            }
            if let Some((n, t)) = sig.params.iter().find(|(_, t)| is_file(t)) {
                return Err(format!("{fpath}'s {n} is a {t}: use --input fd"));
            }
            let max = if o.input == Input::Bytes { BYTES_MAX } else { TEXT_MAX };
            let mut fields = String::new();
            for (n, t) in &sig.params {
                let ft = owned(t);
                if !is_wire(&ft) && !derives_message(sources, &ft) {
                    notes.push(format!("{ft} (parameter {n}) crosses to the worker: give it #[derive(Message)] and #[message(version = 1)]"));
                }
                fields.push_str(&bound_attr(&ft, max));
                fields.push_str(&format!("    pub {n}: {ft},\n"));
            }
            let names: Vec<&str> = sig.params.iter().map(|(n, _)| n.as_str()).collect();
            let args: Vec<String> = sig.params.iter().map(|(n, t)| to_arg(n, t)).collect();
            let destructure = if names.is_empty() { String::new() } else { format!("    let {p}Ask {{ {} }} = ask;\n", names.join(", ")) };
            let ask_arg = if names.is_empty() { "_ask" } else { "ask" };
            let params: Vec<String> = sig.params.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            let build: Vec<String> = sig.params.iter().map(|(n, t)| {
                let v = to_field(n, t);
                if v == *n { n.clone() } else { format!("{n}: {v}") }
            }).collect();
            let ask = if build.is_empty() { format!("{p}Ask {{}}") } else { format!("{p}Ask {{ {} }}", build.join(", ")) };
            let default = if sig.params.iter().all(|(_, t)| is_wire(&owned(t))) { DERIVE_DEFAULT } else { DERIVE };
            Ok(format!(
                "/// What the core asks: {fpath}'s arguments.\n{default}pub struct {p}Ask {{\n{fields}}}\n\n{answer_def}\n\
                 /// The worker: {fpath}, with no capabilities.\n\
                 pub fn {snake}({ask_arg}: {p}Ask, _file: Option<File>) -> Result<{p}Answer, String> {{\n{destructure}{}}}\n\n{}",
                answer(&run(&args.join(", "))),
                call_fn(name, &params.join(", "), &ask, &ret, &map, "None")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let s = parse_sig("fn feed<'a>(x: u8)").unwrap_err();
        assert!(s.contains("generic"), "{s}");
        let s = parse_sig("fn feed(bytes: &'a [u8], max: usize) -> Result<Vec<Item>, ParseError> {").unwrap();
        assert_eq!(s.params, vec![("bytes".into(), "&[u8]".into()), ("max".into(), "usize".into())]);
        assert_eq!(s.ret, "Result<Vec<Item>, ParseError>");
        assert_eq!(split_result(&s.ret), ("Vec<Item>".into(), Some("ParseError".into())));
        let s = parse_sig("fn f(\n    mut a: String,\n    _: (u8, u8),\n) {").unwrap();
        assert_eq!(s.params, vec![("a".into(), "String".into()), ("arg1".into(), "(u8, u8)".into())]);
        assert_eq!(s.ret, "()");
        assert!(parse_sig("fn f(&self) -> u8 {").is_err());
        assert!(parse_sig("fn f(r: impl Read) -> u8 {").is_err());
    }

    #[test]
    fn call_sites() {
        assert!(calls_fn("    let x = parse::feed(&b);", "feed"));
        assert!(!calls_fn("    let x = feeder(&b);", "feed"));
        assert!(!calls_fn("    let feed = 3;", "feed"));
        assert!(is_definition("pub fn feed(x: u8) {", "feed"));
        assert!(!is_definition("pub fn feeder(x: u8) {", "feed"));
    }

    #[test]
    fn plan_on_app_template() {
        let dir = std::env::temp_dir().join(format!("atrium-worker-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let o = crate::new::Opts { name: "demo", template: "app", id: None, sdk_path: None, registry: crate::new::REGISTRY };
        crate::new::create(&dir, &o).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub mod parse {\n    pub fn shout(text: &str, times: u32) -> String {\n        text.repeat(times as usize)\n    }\n}\n").unwrap();
        let a = AddOpts { name: "shout", input: Input::Message, from: Some("demo::parse::shout"), lifetime: "call" };
        let p = plan(&dir, &a).unwrap();
        apply(&dir, &p).unwrap();
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        let m = read("atrium.toml");
        assert!(m.contains("name = \"shout\"\nentry = \"bin/demo-shout\"\ninput = \"message\""), "{m}");
        let (findings, _, _) = crate::check::manifest(&m);
        assert!(findings.iter().all(|x| x.level != crate::check::Level::Error), "{findings:?}");
        let c = read("Cargo.toml");
        assert!(c.contains("atrium-worker = { version = \"0.1\", registry = \"atrium\" }"), "{c}");
        let targets = crate::fuzz::targets(&c, Some(&m)).unwrap();
        assert_eq!(targets.iter().map(|t| t.function.as_str()).collect::<Vec<_>>(), ["demo::workers::shout"]);
        assert!(read("src/lib.rs").contains("pub mod workers {\n    pub use crate::shout_worker::shout;\n}"));
        let w = read("src/shout_worker.rs");
        assert!(w.contains("pub fn call(text: &str, times: u32) -> Result<String, String>"), "{w}");
        assert!(w.contains("ShoutAsk { text: text.to_string(), times }"), "{w}");
        assert_eq!(read("src/bin/shout.rs").lines().last().unwrap(), "atrium_worker::main!(demo::workers::shout);");
        // A second one goes in after the first; the same name is refused.
        let p = plan(&dir, &AddOpts { name: "thumb", input: Input::Fd, from: None, lifetime: "call" }).unwrap();
        apply(&dir, &p).unwrap();
        assert!(read("src/lib.rs").contains("    pub use crate::shout_worker::shout;\n    pub use crate::thumb_worker::thumb;\n"));
        assert!(plan(&dir, &AddOpts { name: "thumb", input: Input::Fd, from: None, lifetime: "call" }).is_err());
        // A function in the core binary can't move as is.
        std::fs::write(dir.join("src/main.rs"), "fn tally(b: &[u8]) -> usize { b.len() }\nfn main() {}\n").unwrap();
        let e = plan(&dir, &AddOpts { name: "tally", input: Input::Bytes, from: Some("tally"), lifetime: "call" }).unwrap_err();
        assert!(e.contains("src/main.rs, a binary"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
