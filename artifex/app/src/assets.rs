//! The Assets view's model (dev-tools.md §3.5, step 4): the project's
//! `assets/` (icons in `assets/icons/`, pictures beside them), previews
//! decoded by the system's image decoder (never in Artifex), where each is
//! used, and importing an SVG through `cargo atrium icon import`, which
//! runs in the workspace: Artifex parses no picture of its own.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use pergola::node::Image;

/// The most files the view lists.
pub const MAX_LISTED: usize = 500;
/// The most folders it walks for them (and for SVGs to import).
pub const MAX_DIRS: usize = 400;
/// The sizes an icon is shown at, in points.
pub const ICON_SIZES: [u32; 4] = [16, 24, 32, 48];
/// The box a picture is fitted into, in points.
pub const PICTURE_BOX: (u32, u32) = (480, 320);
/// Pixels per point the previews are decoded at.
pub const DENSITY: u32 = 2;
/// The most source files searched for an asset's uses, the most bytes read
/// of each, and the most uses kept.
pub const USE_FILES: usize = 2000;
pub const USE_BYTES: u64 = 512 << 10;
pub const USES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An SVG in `assets/icons/`: checked against the dock's limits.
    Icon,
    /// What the decoder opens: PNG, JPEG, GIF, WebP, SVG.
    Picture,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    /// From the project, e.g. `assets/icons/app.svg`.
    pub rel: String,
    pub kind: Kind,
    pub bytes: u64,
}

impl Asset {
    /// The name `atrium_assets::image` opens it by (`icons/app.svg`).
    pub fn name(&self) -> &str {
        self.rel.strip_prefix("assets/").unwrap_or(&self.rel)
    }
}

/// `cargo atrium icon check --json` / the report of an import.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IconReport {
    pub ok: bool,
    pub shapes: u64,
    pub points: u64,
    pub bytes: u64,
    pub problem: Option<String>,
    /// The dock's limits: shapes, points, bytes.
    pub limits: (u64, u64, u64),
}

/// What `cargo atrium icon import --dry-run --json` says it would do.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportPlan {
    /// The SVG imported, from the project.
    pub src: String,
    pub path: String,
    pub replaces: bool,
    pub report: IconReport,
    pub converted: Vec<String>,
    pub dropped: Vec<String>,
    /// The icon it would write (for the preview).
    pub svg: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Importing {
    /// The project's SVGs outside `assets/`, to pick one from.
    Choosing(Vec<String>),
    /// Asking cargo atrium about one.
    Asking(String),
    Planned(ImportPlan),
    Failed(String),
}

/// A preview: decoded pictures, or why there are none.
#[derive(Clone)]
pub struct Preview {
    pub rel: String,
    /// Icons: one a size of [`ICON_SIZES`]. Pictures: one, fitted.
    pub images: Vec<Result<Image, String>>,
    /// `file:line` and the line, where the project names it.
    pub uses: Option<Vec<(String, usize, String)>>,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown: Vec<String> = self.images.iter().map(|i| match i {
            Ok(img) => format!("{}x{}", img.data().width, img.data().height),
            Err(e) => format!("error {e}"),
        }).collect();
        write!(f, "Preview {{ rel: {:?}, images: {shown:?}, uses: {:?} }}", self.rel, self.uses)
    }
}

impl PartialEq for Preview {
    fn eq(&self, o: &Preview) -> bool {
        self.rel == o.rel && self.uses == o.uses && self.images.len() == o.images.len()
    }
}

#[derive(Clone, Default)]
pub struct Assets {
    pub project: String,
    pub files: Vec<Asset>,
    /// More than [`MAX_LISTED`], or a folder left unread.
    pub truncated: bool,
    pub error: Option<String>,
    pub selected: usize,
    pub preview: Option<Preview>,
    /// The selected icon's check (rel, report or error).
    pub check: Option<(String, Result<IconReport, String>)>,
    pub importing: Option<Importing>,
    /// The planned import's icon, decoded at [`ICON_SIZES`].
    pub import_preview: Option<Vec<Result<Image, String>>>,
}

impl std::fmt::Debug for Assets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assets").field("project", &self.project).field("files", &self.files.len())
            .field("selected", &self.selected).field("preview", &self.preview).field("check", &self.check)
            .field("importing", &self.importing).field("import_preview", &self.import_preview.as_ref().map(Vec::len)).finish()
    }
}

impl PartialEq for Assets {
    fn eq(&self, o: &Assets) -> bool {
        self.project == o.project && self.files == o.files && self.truncated == o.truncated && self.error == o.error
            && self.selected == o.selected && self.preview == o.preview && self.check == o.check
            && self.importing == o.importing && self.import_preview.as_ref().map(Vec::len) == o.import_preview.as_ref().map(Vec::len)
    }
}

impl Assets {
    pub fn selected(&self) -> Option<&Asset> {
        self.files.get(self.selected)
    }
}

fn kind_of(rel: &str) -> Kind {
    let lower = rel.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if lower.starts_with("assets/icons/") && ext == "svg" {
        Kind::Icon
    } else if ["png", "jpg", "jpeg", "gif", "webp", "svg"].contains(&ext) {
        Kind::Picture
    } else {
        Kind::Other
    }
}

/// Walk `dir` (not following links), at most [`MAX_DIRS`] folders, calling
/// `f(rel, len)` for each file; `skip` names folders not entered. False when
/// the walk stopped early.
fn walk(dir: &Path, rel: &str, skip: &[&str], f: &mut dyn FnMut(&str, u64) -> bool) -> bool {
    let mut stack = vec![(dir.to_path_buf(), rel.to_string())];
    let mut dirs = 0;
    let mut whole = true;
    while let Some((d, r)) = stack.pop() {
        dirs += 1;
        if dirs > MAX_DIRS {
            return false;
        }
        let Ok(rd) = std::fs::read_dir(&d) else {
            whole = false;
            continue;
        };
        let mut ents: Vec<(String, std::fs::FileType, u64)> = rd.flatten().filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let t = e.file_type().ok()?;
            let len = if t.is_file() { e.metadata().map(|m| m.len()).unwrap_or(0) } else { 0 };
            Some((name, t, len))
        }).collect();
        ents.sort_by(|a, b| b.0.cmp(&a.0));
        for (name, t, len) in ents {
            if name.starts_with('.') {
                continue;
            }
            let rr = if r.is_empty() { name.clone() } else { format!("{r}/{name}") };
            if t.is_dir() {
                if !skip.contains(&rr.as_str()) && !skip.contains(&name.as_str()) {
                    stack.push((d.join(&name), rr));
                }
            } else if t.is_file() && !f(&rr, len) {
                return false;
            }
        }
    }
    whole
}

/// The project's assets (`dir` is the project's folder).
pub fn list(project: &str, dir: &Path) -> Assets {
    let mut a = Assets { project: project.to_string(), ..Default::default() };
    let root = dir.join("assets");
    match std::fs::symlink_metadata(&root) {
        Err(_) => return a,
        Ok(m) if !m.is_dir() => {
            a.error = Some("assets is not a folder of the project (a link isn't followed)".into());
            return a;
        }
        Ok(_) => {}
    }
    let mut files = Vec::new();
    let whole = walk(&root, "assets", &[], &mut |rel, len| {
        files.push(Asset { rel: rel.to_string(), kind: kind_of(rel), bytes: len });
        files.len() < MAX_LISTED
    });
    a.truncated = !whole;
    // Icons first, then pictures, then the rest; by name within each.
    files.sort_by(|x, y| (x.kind as u8, &x.rel).cmp(&(y.kind as u8, &y.rel)));
    a.files = files;
    a
}

/// SVGs in the project outside `assets/` (and build folders), to import.
pub fn svgs(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(dir, "", &["assets", "target", "node_modules", "build", "fuzz"], &mut |rel, _| {
        if rel.to_ascii_lowercase().ends_with(".svg") {
            out.push(rel.to_string());
        }
        out.len() < 200
    });
    out.sort();
    out
}

/// Where the project's sources name `needle` (an asset's name).
pub fn uses(dir: &Path, needle: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let mut seen = 0;
    walk(dir, "", &["assets", "target", "node_modules", "build"], &mut |rel, len| {
        let ext = rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if !["rs", "c", "h", "cc", "cpp", "hpp", "toml", "md", "txt"].contains(&ext) || len > USE_BYTES {
            return true;
        }
        seen += 1;
        if let Ok(text) = std::fs::read_to_string(dir.join(rel)) {
            for (i, l) in text.lines().enumerate() {
                if l.contains(needle) {
                    out.push((rel.to_string(), i + 1, l.trim().chars().take(120).collect()));
                    if out.len() >= USES {
                        return false;
                    }
                }
            }
        }
        seen < USE_FILES
    });
    out
}

fn json(buf: &[u8]) -> Result<serde_json::Value, String> {
    // The last line that is a JSON object (cargo may print before it).
    let text = String::from_utf8_lossy(buf);
    let line = text.lines().rev().find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| {
            let t = text.trim();
            if t.is_empty() { "cargo atrium said nothing (is it in this workspace?)".to_string() } else { t.chars().take(300).collect() }
        })?;
    serde_json::from_str(line).map_err(|e| format!("cargo atrium's answer: {e}"))
}

fn report(v: &serde_json::Value) -> IconReport {
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let l = |k: &str| v.get("limits").and_then(|x| x.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
    IconReport {
        ok: v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
        shapes: n("shapes"),
        points: n("points"),
        bytes: n("bytes"),
        problem: v.get("problem").and_then(|x| x.as_str()).map(str::to_string),
        limits: (l("shapes"), l("points"), l("bytes")),
    }
}

fn error_of(v: &serde_json::Value) -> Option<String> {
    v.get("error").and_then(|x| x.as_str()).map(str::to_string)
}

/// `cargo atrium icon check --json`'s answer.
pub fn read_check(buf: &[u8]) -> Result<IconReport, String> {
    let v = json(buf)?;
    if let Some(e) = error_of(&v) {
        return Err(e);
    }
    Ok(report(&v))
}

/// `cargo atrium icon import --json`'s answer.
pub fn read_import(buf: &[u8], src: &str) -> Result<ImportPlan, String> {
    let v = json(buf)?;
    if let Some(e) = error_of(&v) {
        return Err(e);
    }
    let strings = |k: &str| v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).take(64).collect()).unwrap_or_default();
    Ok(ImportPlan {
        src: src.to_string(),
        path: v.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        replaces: v.get("replaces").and_then(|x| x.as_bool()).unwrap_or(false),
        report: v.get("report").map(report).unwrap_or_default(),
        converted: strings("converted"),
        dropped: strings("dropped"),
        svg: v.get("svg").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

/// The argv for an icon check.
pub fn check_argv(file: &str) -> Vec<String> {
    ["cargo", "atrium", "icon", "check", file, "--json"].iter().map(|a| a.to_string()).collect()
}

/// The argv for an import (a preview with `dry`).
pub fn import_argv(file: &str, dry: bool) -> Vec<String> {
    let mut v: Vec<String> = ["cargo", "atrium", "icon", "import", file, "--json"].iter().map(|a| a.to_string()).collect();
    if dry {
        v.push("--dry-run".into());
    }
    v
}

/// What the decoding thread is asked: a file (or SVG text) at boxes.
pub enum Source {
    File(PathBuf),
    Svg(String),
}

pub struct Request {
    pub tag: String,
    pub source: Source,
    /// Boxes in pixels.
    pub boxes: Vec<(u32, u32)>,
    /// Search the project at `dir` for this name too.
    pub uses: Option<(PathBuf, String)>,
}

pub struct Answer {
    pub tag: String,
    pub images: Vec<Result<Image, String>>,
    pub uses: Option<Vec<(String, usize, String)>>,
}

/// A thread with one decoder session, made when first needed and again
/// after a failure (atrium_assets::Decoder is done after one).
pub fn start() -> (mpsc::Sender<Request>, mpsc::Receiver<Answer>) {
    let (tx, rx) = mpsc::channel::<Request>();
    let (atx, arx) = mpsc::channel::<Answer>();
    let _ = std::thread::Builder::new().name("artifex-assets".into()).spawn(move || {
        let mut decoder: Option<atrium_assets::Decoder> = None;
        while let Ok(mut req) = rx.recv() {
            // Only the newest ask matters (the person moved on).
            while let Ok(newer) = rx.try_recv() {
                req = newer;
            }
            let mut images = Vec::new();
            for &(w, h) in &req.boxes {
                images.push(decode_one(&mut decoder, &req.source, w, h));
            }
            let uses = req.uses.as_ref().map(|(dir, name)| uses(dir, name));
            if atx.send(Answer { tag: req.tag, images, uses }).is_err() {
                return;
            }
        }
    });
    (tx, arx)
}

fn decode_one(decoder: &mut Option<atrium_assets::Decoder>, src: &Source, w: u32, h: u32) -> Result<Image, String> {
    // A session that has been idle may be gone (portcullisd ends a system
    // worker's jail after a while): a failure on one made earlier is tried
    // once more on a fresh one.
    let reused = decoder.is_some();
    match decode_once(decoder, src, w, h) {
        Err(Retry(_)) if reused && decoder.is_none() => decode_once(decoder, src, w, h).map_err(|Retry(e)| e),
        r => r.map_err(|Retry(e)| e),
    }
}

/// Why a picture wasn't decoded (tried again when it ended the session).
struct Retry(String);

fn decode_once(decoder: &mut Option<atrium_assets::Decoder>, src: &Source, w: u32, h: u32) -> Result<Image, Retry> {
    let fail = |e: String| Retry(e);
    let file = match src {
        Source::File(p) => {
            // Not through a link: the decoder gets the project's own file.
            match std::fs::symlink_metadata(p) {
                Ok(m) if m.is_file() => {}
                Ok(_) => return Err(fail("not a file of the project".into())),
                Err(e) => return Err(fail(e.to_string())),
            }
            std::fs::File::open(p).map_err(|e| fail(e.to_string()))?
        }
        Source::Svg(text) => svg_file(text).map_err(fail)?,
    };
    if decoder.is_none() {
        *decoder = Some(atrium_assets::Decoder::new().map_err(|e| fail(e.to_string()))?);
    }
    let d = decoder.as_mut().ok_or_else(|| fail("no decoder".into()))?;
    match d.decode(file, w, h) {
        Ok(p) => {
            if p.width == 0 || p.height == 0 {
                return Err(fail("an empty picture".into()));
            }
            Ok(Image::new(p.width, p.height, p.rgba).with_density(DENSITY as f32))
        }
        Err(e) => {
            // A refusal of this file (the decoder answered) keeps the
            // session; anything else ends it.
            if !matches!(e, atrium_assets::Error::Decoder(atrium_worker::Error::Worker(_))) {
                *decoder = None;
            }
            Err(fail(e.to_string()))
        }
    }
}

/// `text` for the decoder, through a pipe: Artifex's jail has no scratch
/// folder, and the decoder reads its input to the end. A thread writes it,
/// so a large text never blocks this one.
fn svg_file(text: &str) -> Result<std::fs::File, String> {
    use std::io::Write;
    let (r, mut w) = std::io::pipe().map_err(|e| e.to_string())?;
    let bytes = text.as_bytes().to_vec();
    std::thread::Builder::new().name("artifex-svg-pipe".into()).spawn(move || {
        let _ = w.write_all(&bytes);
    }).map_err(|e| e.to_string())?;
    Ok(std::fs::File::from(std::os::fd::OwnedFd::from(r)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("artifex-assets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("assets/icons")).unwrap();
        d
    }

    #[test]
    fn lists_icons_then_pictures() {
        let d = dir("list");
        std::fs::write(d.join("assets/icons/app.svg"), "<svg/>").unwrap();
        std::fs::write(d.join("assets/photo.png"), [0u8; 10]).unwrap();
        std::fs::write(d.join("assets/notes.txt"), "x").unwrap();
        std::fs::write(d.join("assets/.hidden"), "x").unwrap();
        let a = list("p", &d);
        let got: Vec<(&str, Kind, u64)> = a.files.iter().map(|f| (f.rel.as_str(), f.kind, f.bytes)).collect();
        assert_eq!(got, [("assets/icons/app.svg", Kind::Icon, 6), ("assets/photo.png", Kind::Picture, 10), ("assets/notes.txt", Kind::Other, 1)]);
        assert_eq!(a.files[0].name(), "icons/app.svg");
        assert!(!a.truncated && a.error.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_assets_folder_is_not_followed() {
        let d = dir("link");
        std::fs::remove_dir_all(d.join("assets")).unwrap();
        std::os::unix::fs::symlink("/etc", d.join("assets")).unwrap();
        let a = list("p", &d);
        assert!(a.files.is_empty() && a.error.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn finds_svgs_to_import_and_uses() {
        let d = dir("svgs");
        std::fs::create_dir_all(d.join("art")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("art/Logo.SVG"), "<svg/>").unwrap();
        std::fs::write(d.join("target/x.svg"), "<svg/>").unwrap();
        std::fs::write(d.join("assets/icons/app.svg"), "<svg/>").unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {\n    let i = atrium_assets::image(\"icons/app.svg\", 48, 48);\n}\n").unwrap();
        assert_eq!(svgs(&d), ["art/Logo.SVG"]);
        assert_eq!(uses(&d, "icons/app.svg"), [("src/main.rs".to_string(), 2, "let i = atrium_assets::image(\"icons/app.svg\", 48, 48);".to_string())]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn svg_text_reaches_the_decoder_whole() {
        use std::io::Read;
        let text = "<svg>".to_string() + &"x".repeat(200_000) + "</svg>";
        let mut got = String::new();
        svg_file(&text).unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, text);
    }

    #[test]
    fn reads_cargo_atrium() {
        let c = read_check(br#"{"ok":false,"shapes":70,"points":10,"bytes":900,"problem":"70 shapes; the dock draws 64","limits":{"shapes":64,"points":8192,"bytes":32768}}"#).unwrap();
        assert_eq!((c.ok, c.shapes, c.limits), (false, 70, (64, 8192, 32768)));
        assert_eq!(c.problem.as_deref(), Some("70 shapes; the dock draws 64"));
        let p = read_import(b"warning: x\n{\"ok\":true,\"dry_run\":true,\"path\":\"assets/icons/logo.svg\",\"replaces\":false,\"report\":{\"ok\":true,\"shapes\":2,\"points\":8,\"bytes\":300,\"problem\":null,\"limits\":{\"shapes\":64,\"points\":8192,\"bytes\":32768}},\"converted\":[\"transforms\"],\"dropped\":[\"text\"],\"svg\":\"<svg/>\"}", "art/logo.svg").unwrap();
        assert_eq!((p.path.as_str(), p.report.shapes, p.converted.len(), p.dropped[0].as_str(), p.svg.as_str()), ("assets/icons/logo.svg", 2, 1, "text", "<svg/>"));
        assert_eq!(read_import(br#"{"ok":false,"error":"not an SVG"}"#, "x").unwrap_err(), "not an SVG");
        assert!(read_check(b"").unwrap_err().contains("said nothing"));
    }
}
