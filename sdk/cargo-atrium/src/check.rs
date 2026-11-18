//! `cargo atrium check` (dev-tools.md §5.5, app-composition.md §6, C9).
//!
//! Three things, all warnings to the developer, never install blockers:
//!
//! 1. **The manifest**: what the validator refuses or warns about,
//!    restricted capabilities (only the system grants them), and the
//!    consent text the person will see at install.
//! 2. **The crate graph**: which known parsers (images, documents,
//!    archives, media, fonts, mail, untrusted serialization) and network
//!    clients each binary links, and whether the binary that links them is
//!    the **core**, the process that holds the manifest's grants. A parser
//!    in the core of an app that reaches the network or the person's files
//!    is the pattern app-composition.md exists to avoid; the finding says
//!    where to move it ("decode in a worker (template: viewer)").
//! 3. **Grants that look unused**: `net-fetch` without the Nuntius client,
//!    `graphics` without Fresco or Pergola.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level:   Level,
    /// Stable id, for Artifex and for silencing.
    pub code:    &'static str,
    pub message: String,
    /// What to do about it.
    pub fix:     Option<String>,
}

impl Finding {
    fn new(level: Level, code: &'static str, message: String, fix: Option<String>) -> Self {
        Finding { level, code, message, fix }
    }
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "level": match self.level { Level::Error => "error", Level::Warning => "warning", Level::Note => "note" },
            "code": self.code, "message": self.message, "fix": self.fix,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Document,
    Archive,
    Media,
    Font,
    Mail,
    Serialization,
    Network,
}

impl Kind {
    fn words(self) -> &'static str {
        match self {
            Kind::Image => "an image decoder",
            Kind::Document => "a document parser",
            Kind::Archive => "an archive or compression decoder",
            Kind::Media => "a media demuxer or decoder",
            Kind::Font => "a font parser",
            Kind::Mail => "a mail parser",
            Kind::Serialization => "a parser for a binary or YAML format",
            Kind::Network => "a network client",
        }
    }
    fn template(self) -> &'static str {
        match self {
            Kind::Media => "media",
            Kind::Mail | Kind::Serialization | Kind::Network => "client",
            _ => "viewer",
        }
    }
}

/// The curated list (app-composition.md §6): crates that parse what
/// arrives from outside. Kept short and well known; a crate missing here
/// is not a crate cleared.
pub const PARSERS: &[(&str, Kind)] = &[
    ("image", Kind::Image), ("png", Kind::Image), ("jpeg-decoder", Kind::Image), ("zune-jpeg", Kind::Image),
    ("zune-png", Kind::Image), ("gif", Kind::Image), ("tiff", Kind::Image), ("image-webp", Kind::Image),
    ("webp", Kind::Image), ("libwebp-sys", Kind::Image), ("qoi", Kind::Image), ("exr", Kind::Image),
    ("kamadak-exif", Kind::Image), ("resvg", Kind::Image), ("usvg", Kind::Image), ("avif-decode", Kind::Image),
    ("ravif", Kind::Image), ("libheif-rs", Kind::Image), ("navigator-image", Kind::Image),
    ("lopdf", Kind::Document), ("pdf", Kind::Document), ("pdf-extract", Kind::Document), ("pdfium-render", Kind::Document),
    ("quick-xml", Kind::Document), ("xml-rs", Kind::Document), ("roxmltree", Kind::Document), ("xmltree", Kind::Document),
    ("html5ever", Kind::Document), ("scraper", Kind::Document), ("kuchiki", Kind::Document), ("lol_html", Kind::Document),
    ("pulldown-cmark", Kind::Document), ("comrak", Kind::Document), ("markdown", Kind::Document),
    ("calamine", Kind::Document), ("docx-rs", Kind::Document), ("rtf-parser", Kind::Document), ("epub", Kind::Document),
    ("zip", Kind::Archive), ("tar", Kind::Archive), ("flate2", Kind::Archive), ("brotli", Kind::Archive),
    ("brotli-decompressor", Kind::Archive), ("zstd", Kind::Archive), ("xz2", Kind::Archive), ("lzma-rs", Kind::Archive),
    ("bzip2", Kind::Archive), ("lz4_flex", Kind::Archive), ("sevenz-rust", Kind::Archive), ("unrar", Kind::Archive),
    ("symphonia", Kind::Media), ("ffmpeg-next", Kind::Media), ("ffmpeg-sys-next", Kind::Media), ("mp4", Kind::Media),
    ("mp4parse", Kind::Media), ("matroska", Kind::Media), ("lewton", Kind::Media), ("claxon", Kind::Media),
    ("hound", Kind::Media), ("minimp3", Kind::Media), ("rodio", Kind::Media), ("gstreamer", Kind::Media),
    ("ttf-parser", Kind::Font), ("rustybuzz", Kind::Font), ("fontdue", Kind::Font), ("ab_glyph", Kind::Font),
    ("swash", Kind::Font), ("font-kit", Kind::Font), ("freetype-rs", Kind::Font), ("allsorts", Kind::Font),
    ("mail-parser", Kind::Mail), ("mailparse", Kind::Mail), ("email-parser", Kind::Mail),
    ("serde_yaml", Kind::Serialization), ("serde_yml", Kind::Serialization), ("yaml-rust", Kind::Serialization),
    ("rmp-serde", Kind::Serialization), ("ciborium", Kind::Serialization), ("serde_cbor", Kind::Serialization),
    ("bincode", Kind::Serialization), ("prost", Kind::Serialization), ("protobuf", Kind::Serialization),
    ("capnp", Kind::Serialization), ("bson", Kind::Serialization),
    ("reqwest", Kind::Network), ("hyper", Kind::Network), ("ureq", Kind::Network), ("curl", Kind::Network),
    ("isahc", Kind::Network), ("attohttpc", Kind::Network), ("surf", Kind::Network), ("h2", Kind::Network),
    ("tungstenite", Kind::Network), ("tokio-tungstenite", Kind::Network), ("lettre", Kind::Network),
    ("async-imap", Kind::Network), ("imap", Kind::Network), ("russh", Kind::Network), ("ssh2", Kind::Network),
];

/// The SDK's own crates: what they link is the platform's choice, used
/// for the platform's purposes (Pergola shapes text with swash on the
/// system's fonts), not a parser the app put in its core.
pub const SDK_CRATES: &[&str] = &["pergola", "fresco-client", "fresco-protocol", "atrium-message", "atrium-worker",
    "limen", "tabula", "aqueduct", "libatrium", "atrium-fonts", "nuntius", "pergola-document", "pergola-richtext"];

/// First-party crates that only read fonts (the system's, or ones an app
/// ships) for drawing text. What only these reach (font parsers, and
/// WOFF's flate2 and brotli) is system-font input, not hostile input.
pub const TEXT_STACK: &[&str] = &["fresco-text", "navigator-fonts", "folium-font", "atrium-colr", "atrium-fonts"];

/// First-party crates that lay out text with a font parser on those same
/// fonts, but parse other things too: they don't make a font parser
/// the app's, while what else they link still counts.
pub const FONT_USERS: &[&str] = &["officina-text", "officina-pict", "officina-draw", "navigator-render", "folium-render",
    "pergola-richtext", "pergola-document"];

/// A crate a binary links.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Linked {
    pub version: String,
    /// The binary's own package depends on it directly.
    pub own: bool,
    /// Other crates that bring it in on a path that doesn't run through
    /// the font stack ([`TEXT_STACK`] and the font parsers): empty, and
    /// not `own`, when only the font stack reaches it.
    pub outside: BTreeSet<String>,
}

/// The font stack: the text crates and the font parsers.
fn font_stack(name: &str) -> bool { TEXT_STACK.contains(&name) || kind_of(name) == Some(Kind::Font) }

pub fn kind_of(name: &str) -> Option<Kind> { PARSERS.iter().find(|(n, _)| *n == name).map(|(_, k)| *k) }

/// What a manifest says the app holds, as the lint needs it.
#[derive(Debug, Default, Clone)]
pub struct Reach {
    pub app_id:   String,
    /// The core's binary (the entry's file name).
    pub core:     String,
    /// Worker binaries (entries of `[[workers]]`), by file name.
    pub workers:  Vec<(String, String)>,
    /// "network" (full or granted), "net-fetch", "files" (filesystem,
    /// opens, home files), each a reason the core is worth guarding.
    pub holds:    Vec<String>,
    pub net_fetch: bool,
    pub network:   bool,
    pub graphics:  bool,
}

fn basename(p: &str) -> String { p.rsplit('/').next().unwrap_or(p).to_string() }

/// Check `atrium.toml`: findings, the consent lines, and the reach.
pub fn manifest(text: &str) -> (Vec<Finding>, Vec<String>, Option<Reach>) {
    let mut f = Vec::new();
    // `[[workers]]` (app-composition.md §4.1) is read here from the raw
    // TOML, so the lint knows which binaries are workers even before the
    // manifest parser takes the section.
    let raw: Option<toml::Value> = match text.parse::<toml::Value>() {
        Ok(v) => Some(v),
        Err(e) => {
            f.push(Finding::new(Level::Error, "manifest-syntax", format!("atrium.toml: {e}"), None));
            return (f, vec![], None);
        }
    };
    let mut workers = Vec::new();
    if let Some(ws) = raw.as_ref().and_then(|v| v.get("workers")).and_then(|w| w.as_array()) {
        for w in ws {
            let name = w.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let entry = w.get("entry").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if name.is_empty() || entry.is_empty() {
                f.push(Finding::new(Level::Error, "worker-incomplete", "a [[workers]] entry needs name and entry".into(), None));
            }
            if w.get("capabilities").is_some() {
                f.push(Finding::new(Level::Error, "worker-capabilities",
                    format!("worker {name:?} declares capabilities; a worker never holds any (C3)"),
                    Some("move whatever needs reach into the core or a system service".into())));
            }
            workers.push((name, basename(&entry)));
        }
    }
    let typed_text = match raw.as_ref() {
        Some(toml::Value::Table(t)) if t.contains_key("workers") || t.contains_key("roles") || t.contains_key("ipc") => {
            let mut t = t.clone();
            t.remove("workers");
            t.remove("roles");
            t.remove("ipc");
            toml::to_string(&toml::Value::Table(t)).unwrap_or_default()
        }
        _ => text.to_string(),
    };
    let m = match portcullis_toml::Manifest::from_str(&typed_text) {
        Ok(m) => m,
        Err(e) => {
            f.push(Finding::new(Level::Error, "manifest-schema", format!("atrium.toml: {}", e.message()), None));
            return (f, vec![], None);
        }
    };
    let r = portcullis_toml::validate(&m);
    for e in &r.errors {
        f.push(Finding::new(Level::Error, "manifest-invalid", e.clone(), None));
    }
    // org.atrium.* is the platform key's prefix (opifex.md §6.5): no third
    // party can install under it, and the system policy grants what it
    // asks. What would be a warning for anyone else is how it works.
    let platform = m.app.id.starts_with("org.atrium.");
    for w in r.warnings.iter().filter(|_| !platform) {
        f.push(Finding::new(Level::Warning, "manifest-warning", w.clone(), None));
    }
    for c in m.capabilities.restricted().into_iter().filter(|_| !platform) {
        f.push(Finding::new(Level::Warning, "restricted-capability",
            format!("{c} is restricted: only the system policy grants it, never a person at install"),
            Some("drop it, or use the system service that holds it".into())));
    }
    let consent = {
        let d = portcullis_policy::compute_delta(&m.capabilities, None, None, "");
        let mut v = d.describe();
        v.extend(m.opens_lines());
        v.extend(m.links_line());
        v
    };
    let c = &m.capabilities;
    let network = c.network.as_ref().is_some_and(|n| n.mode() == portcullis_toml::NetworkCap::Full);
    let net_fetch = c.net_fetch == Some(true);
    let mut holds = Vec::new();
    if network {
        holds.push("network".to_string());
    }
    if net_fetch {
        holds.push("net-fetch".to_string());
    }
    if c.filesystem.as_ref().is_some_and(|v| !v.is_empty()) || !m.opens.is_empty() || c.home_files == Some(true) {
        holds.push("your files".to_string());
    }
    let reach = Reach {
        app_id: m.app.id.clone(),
        core: basename(m.entry()),
        workers,
        holds,
        net_fetch,
        network,
        graphics: c.graphics.is_some(),
    };
    (f, consent, Some(reach))
}

/// The crate graph from `cargo metadata --format-version 1`: for each
/// binary target of the workspace's own packages, every crate it links
/// (normal dependencies, transitively), name → version.
pub fn binaries(metadata: &Value) -> Result<BTreeMap<String, BTreeMap<String, Linked>>, String> {
    let pkgs = metadata.get("packages").and_then(Value::as_array).ok_or("metadata: packages")?;
    let members: BTreeSet<&str> = metadata.get("workspace_members").and_then(Value::as_array).ok_or("metadata: workspace_members")?
        .iter().filter_map(Value::as_str).collect();
    let mut by_id: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for p in pkgs {
        let id = p.get("id").and_then(Value::as_str).unwrap_or("");
        by_id.insert(id, (p.get("name").and_then(Value::as_str).unwrap_or(""), p.get("version").and_then(Value::as_str).unwrap_or("")));
    }
    let nodes = metadata.get("resolve").and_then(|r| r.get("nodes")).and_then(Value::as_array).ok_or("metadata: resolve (run without --no-deps)")?;
    let mut deps: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for n in nodes {
        let id = n.get("id").and_then(Value::as_str).unwrap_or("");
        let mut v = Vec::new();
        for d in n.get("deps").and_then(Value::as_array).into_iter().flatten() {
            let normal = d.get("dep_kinds").and_then(Value::as_array)
                .is_none_or(|ks| ks.iter().any(|k| k.get("kind").is_none_or(Value::is_null)));
            if normal {
                if let Some(p) = d.get("pkg").and_then(Value::as_str) {
                    v.push(p);
                }
            }
        }
        deps.insert(id, v);
    }
    let mut out = BTreeMap::new();
    for p in pkgs {
        let id = p.get("id").and_then(Value::as_str).unwrap_or("");
        if !members.contains(id) {
            continue;
        }
        // Everything this package links, from its id; not through the
        // SDK's own crates (they are listed, what they link is not).
        let pname = p.get("name").and_then(Value::as_str).unwrap_or("");
        let sdk = |x: &str| x != id && by_id.get(x).is_some_and(|(n, _)| SDK_CRATES.contains(n));
        let name_of = |x: &str| by_id.get(x).map(|(n, _)| n.to_string()).unwrap_or_default();
        // Everything it links (not through the SDK's own crates).
        let mut seen = BTreeSet::new();
        let mut q = VecDeque::from([id]);
        while let Some(x) = q.pop_front() {
            if sdk(x) {
                continue;
            }
            for d in deps.get(x).into_iter().flatten() {
                if seen.insert(*d) {
                    q.push_back(d);
                }
            }
        }
        // Again, without going through the font stack: who brings each in.
        let mut parents: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        let mut seen2 = BTreeSet::from([id]);
        let mut q = VecDeque::from([id]);
        while let Some(x) = q.pop_front() {
            if sdk(x) || (x != id && font_stack(&name_of(x))) {
                continue;
            }
            for d in deps.get(x).into_iter().flatten() {
                parents.entry(d).or_default().insert(name_of(x));
                if seen2.insert(*d) {
                    q.push_back(d);
                }
            }
        }
        let linked: BTreeMap<String, Linked> = seen.iter().filter_map(|i| by_id.get(i).map(|(n, v)| {
            let ps = parents.get(i).cloned().unwrap_or_default();
            (n.to_string(), Linked {
                version: v.to_string(),
                own: ps.contains(pname),
                outside: ps.into_iter().filter(|x| x != pname).collect(),
            })
        })).collect();
        for t in p.get("targets").and_then(Value::as_array).into_iter().flatten() {
            let is_bin = t.get("kind").and_then(Value::as_array).is_some_and(|k| k.iter().any(|x| x == "bin"));
            if is_bin {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    out.insert(n.to_string(), linked.clone());
                }
            }
        }
    }
    Ok(out)
}

/// The crate-graph findings for an app.
pub fn graph(reach: &Reach, bins: &BTreeMap<String, BTreeMap<String, Linked>>, built_elsewhere: &BTreeSet<String>) -> Vec<Finding> {
    let mut f = Vec::new();
    let Some(core) = bins.get(&reach.core) else {
        f.push(Finding::new(Level::Warning, "core-not-found",
            format!("no binary named {:?} (the manifest's entry) in this workspace", reach.core),
            Some("name the [[bin]] after the entry's file, or fix the entry".into())));
        return f;
    };
    for (_, wbin) in &reach.workers {
        if !bins.contains_key(wbin) && !built_elsewhere.contains(wbin) {
            f.push(Finding::new(Level::Warning, "worker-not-found", format!("no binary named {wbin:?} for a [[workers]] entry"),
                Some("name a [[bin]] after the entry's file, or say which crate builds it under [package.metadata.atrium.workers]".into())));
        }
    }
    let worker_names: Vec<&str> = reach.workers.iter().map(|w| w.0.as_str()).collect();
    for (name, l) in core {
        let Some(kind) = kind_of(name) else { continue };
        let version = &l.version;
        let bin = &reach.core;
        // Reached only through the font stack: system fonts and the
        // fonts apps ship (WOFF's flate2 and brotli included).
        if !l.own && l.outside.is_empty() {
            continue;
        }
        if kind == Kind::Font && l.outside.iter().all(|x| FONT_USERS.contains(&x.as_str())) {
            if !l.own {
                continue;
            }
            f.push(Finding::new(Level::Note, "font-parser-in-core",
                format!("`{bin}` links {name} {version} itself: fine for the fonts it ships or the system's"),
                Some("a font from a document or the network belongs in a worker (template: viewer)".into())));
            continue;
        }
        if kind == Kind::Network {
            let (msg, fix) = if reach.network {
                (format!("`{bin}` links {name} {version} ({}) and holds the network: what it parses arrives in the process holding the app's grants",
                         kind.words()),
                 "fetch through Nuntius (`net-fetch`) and parse the bytes in a worker (template: client)".to_string())
            } else if reach.net_fetch {
                (format!("`{bin}` links {name} {version} beside `net-fetch`: two ways to the network; only Nuntius has the address policy"),
                 "drop the client crate and fetch through Nuntius".to_string())
            } else {
                (format!("`{bin}` links {name} {version} but the manifest grants no network: it will fail at run time"),
                 "fetch through Nuntius with `net-fetch`".to_string())
            };
            f.push(Finding::new(Level::Warning, "network-client-in-core", msg, Some(fix)));
            continue;
        }
        if reach.holds.is_empty() {
            f.push(Finding::new(Level::Note, "parser-in-core",
                format!("`{bin}` links {name} {version} ({}); it holds no network or files today, but it is the process with the window",
                        kind.words()),
                Some(format!("parse in a worker if the input can come from outside (template: {})", kind.template()))));
        } else {
            let what = match kind {
                Kind::Image => "decode",
                Kind::Media => "demux and decode",
                _ => "parse",
            };
            f.push(Finding::new(Level::Warning, "parser-in-core",
                format!("`{}` holds {} and links {name} {version} ({})", reach.app_id, reach.holds.join(" and "), kind.words()),
                Some(format!("{what} in a worker (template: {}){}", kind.template(),
                    if worker_names.is_empty() { String::new() } else { format!("; this app has workers: {}", worker_names.join(", ")) }))));
        }
    }
    if reach.net_fetch && !core.contains_key("nuntius") && !core.contains_key("atrium-nuntius") {
        f.push(Finding::new(Level::Note, "unused-capability",
            "`net-fetch` is declared but the core doesn't link the Nuntius client".into(),
            Some("use it, or drop the capability (the person is asked about it)".into())));
    }
    if reach.graphics && !["fresco-client", "pergola", "libfresco"].iter().any(|c| core.contains_key(*c)) {
        f.push(Finding::new(Level::Note, "unused-capability",
            "`graphics` is declared but the core links neither Pergola nor the Fresco client".into(), None));
    }
    f
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

[[workers]]
name = "decode"
entry = "bin/photos-decode"
input = "fd"
"#;

    fn meta() -> Value {
        // photos (core) -> image -> png; photos -> pergola; photos-decode -> image.
        serde_json::json!({
            "workspace_members": ["photos 1", "decode 1"],
            "packages": [
                {"id": "photos 1", "name": "photos", "version": "1.0.0", "targets": [{"kind": ["bin"], "name": "photos"}]},
                {"id": "decode 1", "name": "photos-decode", "version": "1.0.0", "targets": [{"kind": ["bin"], "name": "photos-decode"}]},
                {"id": "image 1", "name": "image", "version": "0.25.6", "targets": [{"kind": ["lib"], "name": "image"}]},
                {"id": "png 1", "name": "png", "version": "0.17.0", "targets": [{"kind": ["lib"], "name": "png"}]},
                {"id": "pergola 1", "name": "pergola", "version": "0.1.0", "targets": [{"kind": ["lib"], "name": "pergola"}]},
                {"id": "zip 1", "name": "zip", "version": "2.0.0", "targets": [{"kind": ["lib"], "name": "zip"}]},
                {"id": "swash 1", "name": "swash", "version": "0.1.0", "targets": [{"kind": ["lib"], "name": "swash"}]}
            ],
            "resolve": {"nodes": [
                {"id": "photos 1", "deps": [
                    {"pkg": "image 1", "dep_kinds": [{"kind": null}]},
                    {"pkg": "pergola 1", "dep_kinds": [{"kind": null}]},
                    {"pkg": "zip 1", "dep_kinds": [{"kind": "dev"}]}]},
                {"id": "decode 1", "deps": [{"pkg": "image 1", "dep_kinds": [{"kind": null}]}]},
                {"id": "image 1", "deps": [{"pkg": "png 1", "dep_kinds": [{"kind": null}]}]},
                {"id": "png 1", "deps": []}, {"id": "zip 1", "deps": []}, {"id": "swash 1", "deps": []},
                {"id": "pergola 1", "deps": [{"pkg": "swash 1", "dep_kinds": [{"kind": null}]}]}
            ]}
        })
    }

    #[test]
    fn a_parser_in_a_networked_core_is_flagged_with_where_to_move_it() {
        let (f, consent, reach) = manifest(PHOTOS);
        assert!(f.iter().all(|x| x.level != Level::Error), "{f:?}");
        let reach = reach.unwrap();
        assert_eq!(reach.core, "photos");
        assert_eq!(reach.workers, vec![("decode".to_string(), "photos-decode".to_string())]);
        assert!(consent.iter().any(|l| l.contains("graphics")), "{consent:?}");
        let g = graph(&reach, &binaries(&meta()).unwrap(), &BTreeSet::new());
        let p: Vec<&Finding> = g.iter().filter(|x| x.code == "parser-in-core").collect();
        assert_eq!(p.len(), 2, "{g:?}"); // image and png, through image
        assert!(p.iter().all(|x| x.level == Level::Warning));
        assert!(p[0].message.contains("holds net-fetch and links image 0.25.6"), "{}", p[0].message);
        assert_eq!(p[0].fix.as_deref(), Some("decode in a worker (template: viewer); this app has workers: decode"));
        // What the SDK links (Pergola's swash) is not the app's parser.
        assert!(!g.iter().any(|x| x.message.contains("swash")), "{g:?}");
        // A dev-dependency is not linked into the core.
        assert!(!g.iter().any(|x| x.message.contains("zip")));
        // The worker's own image is its business.
        assert!(!g.iter().any(|x| x.message.contains("photos-decode")));
        // net-fetch without the Nuntius client.
        assert!(g.iter().any(|x| x.code == "unused-capability" && x.message.contains("net-fetch")));
    }

    #[test]
    fn the_viewer_template_shape_is_silent() {
        // A core with no parsers, its worker with them.
        let mut m = meta();
        m["resolve"]["nodes"][0]["deps"] = serde_json::json!([{"pkg": "pergola 1", "dep_kinds": [{"kind": null}]}]);
        let (_, _, reach) = manifest(&PHOTOS.replace("net-fetch = true", "filesystem = [\"documents\"]"));
        let g = graph(&reach.unwrap(), &binaries(&m).unwrap(), &BTreeSet::new());
        assert!(g.iter().all(|x| x.level != Level::Warning), "{g:?}");
    }

    #[test]
    fn network_clients_and_restricted_capabilities() {
        let mut m = meta();
        m["packages"][2]["name"] = "reqwest".into();
        let (f, _, reach) = manifest(&PHOTOS.replace("net-fetch = true", "network = \"full\"\nwindow-management = true"));
        assert!(f.iter().any(|x| x.code == "restricted-capability" && x.message.contains("window-management")), "{f:?}");
        let g = graph(&reach.unwrap(), &binaries(&m).unwrap(), &BTreeSet::new());
        assert!(g.iter().any(|x| x.code == "network-client-in-core" && x.message.contains("holds the network")), "{g:?}");
    }

    #[test]
    fn manifest_errors() {
        let (f, _, r) = manifest("[app\n");
        assert!(r.is_none() && f[0].code == "manifest-syntax");
        let (f, _, _) = manifest(&PHOTOS.replace("input = \"fd\"", "input = \"fd\"\ncapabilities = { network = \"full\" }"));
        assert!(f.iter().any(|x| x.code == "worker-capabilities"));
        let (f, _, r) = manifest(&PHOTOS.replace("[capabilities]", "[capabilitiez]"));
        assert!(r.is_none() && f.iter().any(|x| x.code == "manifest-schema"), "{f:?}");
    }

    #[test]
    fn the_font_stack_is_system_font_input() {
        // core -> fresco-text -> rustybuzz -> ttf-parser; fresco-text -> allsorts -> flate2;
        // core -> officina-text -> ttf-parser; core -> navigator-render -> png -> flate2? no:
        // core -> navigator-render -> pulldown-cmark; core -> swash (its own font).
        let node = |id: &str, deps: &[&str]| serde_json::json!({"id": id, "deps":
            deps.iter().map(|d| serde_json::json!({"pkg": d, "dep_kinds": [{"kind": null}]})).collect::<Vec<_>>()});
        let pkg = |id: &str| serde_json::json!({"id": id, "name": id.split(' ').next().unwrap(), "version": "1",
            "targets": if id == "app 1" { serde_json::json!([{"kind": ["bin"], "name": "app"}]) } else { serde_json::json!([]) }});
        let ids = ["app 1", "fresco-text 1", "rustybuzz 1", "ttf-parser 1", "allsorts 1", "flate2 1", "officina-text 1",
                   "navigator-render 1", "pulldown-cmark 1", "swash 1"];
        let m = serde_json::json!({
            "workspace_members": ["app 1"],
            "packages": ids.iter().map(|i| pkg(i)).collect::<Vec<_>>(),
            "resolve": {"nodes": [
                node("app 1", &["fresco-text 1", "officina-text 1", "navigator-render 1", "swash 1"]),
                node("fresco-text 1", &["rustybuzz 1", "allsorts 1"]), node("rustybuzz 1", &["ttf-parser 1"]),
                node("allsorts 1", &["flate2 1"]), node("officina-text 1", &["ttf-parser 1"]),
                node("navigator-render 1", &["pulldown-cmark 1"]),
                node("ttf-parser 1", &[]), node("flate2 1", &[]), node("pulldown-cmark 1", &[]), node("swash 1", &[]),
            ]}
        });
        let (_, _, reach) = manifest(&PHOTOS.replace("net-fetch = true", "filesystem = [\"documents\"]").replace("entry = \"bin/photos\"", "entry = \"bin/app\""));
        let g = graph(&reach.unwrap(), &binaries(&m).unwrap(), &BTreeSet::new());
        let named = |n: &str| g.iter().filter(|x| x.message.contains(n)).collect::<Vec<_>>();
        assert!(named("rustybuzz").is_empty() && named("ttf-parser").is_empty() && named("allsorts").is_empty(), "{g:?}");
        assert!(named("flate2").is_empty(), "WOFF's flate2, only through the font stack: {g:?}");
        assert_eq!(named("pulldown-cmark").len(), 1, "{g:?}");
        let sw = named("swash");
        assert!(sw.len() == 1 && sw[0].level == Level::Note, "{g:?}");
        // flate2 by another path too: flagged.
        let mut m2 = m.clone();
        m2["resolve"]["nodes"][7] = node("navigator-render 1", &["pulldown-cmark 1", "flate2 1"]);
        let (_, _, reach) = manifest(&PHOTOS.replace("net-fetch = true", "filesystem = [\"documents\"]").replace("entry = \"bin/photos\"", "entry = \"bin/app\""));
        let g = graph(&reach.unwrap(), &binaries(&m2).unwrap(), &BTreeSet::new());
        assert!(g.iter().any(|x| x.message.contains("flate2") && x.level == Level::Warning), "{g:?}");
    }

    #[test]
    fn platform_ids_are_quiet_about_restricted_capabilities() {
        let t = PHOTOS.replace("net-fetch = true", "network = \"full\"\nwindow-management = true");
        let (f, _, _) = manifest(&t);
        assert!(f.iter().any(|x| x.code == "restricted-capability"));
        let id = t.lines().find(|l| l.starts_with("id")).unwrap().to_string();
        let (f, _, _) = manifest(&t.replace(&id, "id = \"org.atrium.photos\""));
        assert!(!f.iter().any(|x| x.code == "restricted-capability" || x.code == "manifest-warning"), "{f:?}");
    }
}
