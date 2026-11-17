//! `cargo atrium new` (dev-tools.md §5.5, app-composition.md §4.3): a
//! project from an SDK template.
//!
//! Templates live in `sdk/templates/<name>/` and are built into the tool.
//! `{{name}}`, `{{title}}`, `{{id}}` and `{{registry}}` are filled in;
//! `{{dep:<crate>}}` becomes a registry dependency on the SDK crate, or,
//! with `--sdk-path <atrium checkout>`, a path dependency on it (how the
//! SDK's own tests build templates before the registry is published).

use std::path::{Path, PathBuf};

/// Where the SDK registry's index is (opifex.md §8.4: the catalog's Pages
/// site under /sdk/; a domain later redirects from here).
pub const REGISTRY: &str = "sparse+https://atrium-os.github.io/catalog/sdk/index/";
/// The SDK version templates ask for.
pub const SDK_VERSION: &str = "0.1";

/// (file in the template, where it goes, text).
type File = (&'static str, &'static str, &'static str);

const APP: &[File] = &[
    ("Cargo.toml.tmpl", "Cargo.toml", include_str!("../../templates/app/Cargo.toml.tmpl")),
    ("atrium.toml.tmpl", "atrium.toml", include_str!("../../templates/app/atrium.toml.tmpl")),
    ("src/main.rs", "src/main.rs", include_str!("../../templates/app/src/main.rs")),
    ("gitignore.tmpl", ".gitignore", include_str!("../../templates/app/gitignore.tmpl")),
    ("cargo-config.toml.tmpl", ".cargo/config.toml", include_str!("../../templates/app/cargo-config.toml.tmpl")),
];

/// (template, what it is, its files).
pub const TEMPLATES: &[(&str, &str, &[File])] = &[
    ("app", "a window and nothing else: start here for an app that opens no files and uses no network", APP),
];

/// Where an SDK crate is in an Atrium checkout.
fn sdk_crate_path(root: &Path, krate: &str) -> PathBuf {
    match krate {
        "atrium-message" | "atrium-worker" | "atrium-schema" => root.join("sdk").join(krate),
        other => root.join(other),
    }
}

pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 48 && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub struct Opts<'a> {
    pub name:     &'a str,
    pub template: &'a str,
    pub id:       Option<&'a str>,
    pub sdk_path: Option<&'a Path>,
    pub registry: &'a str,
}

fn title(name: &str) -> String {
    let mut s: String = name.replace('-', " ");
    if let Some(f) = s.get_mut(0..1) {
        f.make_ascii_uppercase();
    }
    s
}

/// The template's files, filled in: (relative path, text).
pub fn render(o: &Opts) -> Result<Vec<(String, String)>, String> {
    if !valid_name(o.name) {
        return Err(format!("{:?}: a name is lowercase letters, digits and dashes", o.name));
    }
    let (_, _, files) = TEMPLATES.iter().find(|t| t.0 == o.template)
        .ok_or_else(|| format!("no template {:?} (cargo atrium new --list)", o.template))?;
    let id = o.id.map(str::to_string).unwrap_or_else(|| format!("org.example.{}", o.name));
    let mut out = Vec::new();
    for (_, to, text) in files.iter() {
        if o.sdk_path.is_some() && *to == ".cargo/config.toml" {
            continue;
        }
        let mut t = text.replace("{{name}}", o.name).replace("{{title}}", &title(o.name))
            .replace("{{id}}", &id).replace("{{registry}}", o.registry);
        while let Some(at) = t.find("{{dep:") {
            let end = t[at..].find("}}").ok_or("a {{dep: without }}")? + at;
            let krate = &t[at + 6..end];
            let dep = match o.sdk_path {
                Some(root) => format!("{{ path = {:?} }}", sdk_crate_path(root, krate).display().to_string()),
                None => format!("{{ version = \"{SDK_VERSION}\", registry = \"atrium\" }}"),
            };
            t.replace_range(at..end + 2, &dep);
        }
        out.push((to.to_string(), t));
    }
    Ok(out)
}

/// Write the project into `dir` (which must not exist, or be empty).
pub fn create(dir: &Path, o: &Opts) -> Result<Vec<String>, String> {
    let files = render(o)?;
    if dir.exists() && std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} exists and isn't empty", dir.display()));
    }
    let mut written = Vec::new();
    for (rel, text) in files {
        let p = dir.join(&rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
        written.push(rel);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_and_path_dependencies() {
        let o = Opts { name: "hello-world", template: "app", id: None, sdk_path: None, registry: REGISTRY };
        let f = render(&o).unwrap();
        let cargo = &f.iter().find(|x| x.0 == "Cargo.toml").unwrap().1;
        assert!(cargo.contains("pergola = { version = \"0.1\", registry = \"atrium\" }"), "{cargo}");
        let m = &f.iter().find(|x| x.0 == "atrium.toml").unwrap().1;
        assert!(m.contains("id = \"org.example.hello-world\"") && m.contains("name = \"Hello world\""));
        assert!(f.iter().any(|x| x.0 == ".cargo/config.toml" && x.1.contains(REGISTRY)));
        assert!(f.iter().all(|x| !x.1.contains("{{")), "unfilled placeholder");
        let root = Path::new("/src/atrium");
        let f = render(&Opts { sdk_path: Some(root), ..o }).unwrap();
        let cargo = &f.iter().find(|x| x.0 == "Cargo.toml").unwrap().1;
        assert!(cargo.contains("pergola = { path = \"/src/atrium/pergola\" }"), "{cargo}");
        assert!(!f.iter().any(|x| x.0 == ".cargo/config.toml"));
        // Its manifest passes check.
        let (findings, consent, _) = crate::check::manifest(m);
        assert!(findings.is_empty(), "{findings:?}");
        assert!(!consent.is_empty());
    }

    #[test]
    fn names() {
        assert!(valid_name("a") && valid_name("photo-viewer2"));
        for n in ["", "A", "-x", "x_y", "x/y", "../x", &"a".repeat(49)] {
            assert!(!valid_name(n), "{n}");
        }
        assert!(render(&Opts { name: "x", template: "nope", id: None, sdk_path: None, registry: REGISTRY }).is_err());
    }
}
