//! Font packages, `kind = "font"` (opifex.md §8.1, fonts.md §5).
//!
//! ```toml
//! kind = "font"
//!
//! [font]
//! id = "org.atrium.font.noto-sans-jp"
//! name = "Noto Sans JP"
//! version = "2.004"
//! license = "OFL-1.1"
//! description = "Japanese, sans serif"
//! languages = ["ja"]
//! content = "sha256:…"            # the content index (opifex pack fills it in)
//!
//! [[face]]
//! family = "Noto Sans JP"
//! file = "NotoSansJP-Regular.ttf"  # at the package's top
//! weight = 400
//! italic = false
//! ranges = "3000-303F,3040-30FF,4E00-9FFF,FF00-FFEF"
//! ```
//!
//! Platform-signed only, OFL or Apache 2.0, static TrueType. A font package
//! is for every machine: its catalog releases carry the arch `any`.

use serde::{Deserialize, Serialize};

pub const LICENCES: [&str; 2] = ["OFL-1.1", "Apache-2.0"];
pub const ARCH: &str = "any";
pub const MAX_FACES: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontManifest {
    pub kind: String,
    pub font: FontSection,
    #[serde(rename = "face")]
    pub faces: Vec<Face>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontSection {
    pub id: String,
    pub name: String,
    pub version: String,
    pub license: String,
    #[serde(default)]
    pub description: String,
    /// BCP 47 languages it is meant for (`ja`, `zh-Hans`): what a local
    /// search for "a Japanese font" matches.
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Face {
    pub family: String,
    pub file: String,
    pub weight: u16,
    #[serde(default)]
    pub italic: bool,
    pub ranges: String,
}

/// `4E00-9FFF,3040` as code point ranges, or None if any part is wrong.
pub fn ranges(s: &str) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let p = |x: &str| (!x.is_empty() && x.len() <= 6).then(|| u32::from_str_radix(x, 16).ok()).flatten();
        let (lo, hi) = (p(a)?, p(b)?);
        if lo > hi || hi > 0x10FFFF {
            return None;
        }
        out.push((lo, hi));
    }
    (!out.is_empty() && out.len() <= 64).then_some(out)
}

/// A package id the pin list takes: `[a-z0-9.-]`, at most 64 bytes, dotted.
pub fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.contains('.') && !id.starts_with('.') && !id.ends_with('.')
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

/// A face file: `<name>.ttf` at the package's top.
pub fn file_ok(f: &str) -> bool {
    f.len() <= 128 && f.len() > 4 && f.ends_with(".ttf") && f != "." && f != ".."
        && f.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

impl FontManifest {
    pub fn from_str(text: &str) -> Result<FontManifest, String> {
        let m: FontManifest = toml::from_str(text).map_err(|e| format!("font manifest: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), String> {
        let f = &self.font;
        if self.kind != "font" {
            return Err("font manifest: kind must be \"font\"".into());
        }
        if !id_ok(&f.id) {
            return Err(format!("font {:?}: id must be [a-z0-9.-], dotted, at most 64 bytes", f.id));
        }
        if f.name.is_empty() || f.name.len() > 64 || f.name.chars().any(char::is_control) {
            return Err(format!("{}: name", f.id));
        }
        if f.version.is_empty() || f.version.len() > 32 || !f.version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)) {
            return Err(format!("{}: version", f.id));
        }
        if !LICENCES.contains(&f.license.as_str()) {
            return Err(format!("{}: licence {:?}: fonts are OFL-1.1 or Apache-2.0", f.id, f.license));
        }
        if self.faces.is_empty() || self.faces.len() > MAX_FACES {
            return Err(format!("{}: 1 to {MAX_FACES} faces", f.id));
        }
        let mut seen = std::collections::HashSet::new();
        for face in &self.faces {
            if face.family.is_empty() || face.family.len() > 64 || face.family.chars().any(char::is_control) {
                return Err(format!("{}: family {:?}", f.id, face.family));
            }
            if !file_ok(&face.file) || !seen.insert(face.file.as_str()) {
                return Err(format!("{}: face file {:?} (a <name>.ttf at the top, once)", f.id, face.file));
            }
            if !(1..=1000).contains(&face.weight) {
                return Err(format!("{}: {}: weight 1 to 1000", f.id, face.file));
            }
            if ranges(&face.ranges).is_none() {
                return Err(format!("{}: {}: ranges {:?}", f.id, face.file, face.ranges));
            }
        }
        Ok(())
    }

    pub fn require_content(&self) -> Result<[u8; 32], String> {
        portcullis_toml::layer::content_digest(&self.font.content)
            .ok_or_else(|| format!("{}: not packed (no content index)", self.font.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> String {
        "kind = \"font\"\n[font]\nid = \"org.atrium.font.test\"\nname = \"Test\"\nversion = \"1.0\"\nlicense = \"OFL-1.1\"\n\
         languages = [\"ja\"]\n\n[[face]]\nfamily = \"Test Sans\"\nfile = \"Test-Regular.ttf\"\nweight = 400\nranges = \"3040-30FF,4E00-9FFF\"\n".into()
    }

    #[test]
    fn reads_and_refuses() {
        let m = FontManifest::from_str(&sample()).unwrap();
        assert_eq!((m.font.id.as_str(), m.faces[0].weight, m.faces[0].italic), ("org.atrium.font.test", 400, false));
        assert_eq!(ranges("3040-30FF,4E00"), Some(vec![(0x3040, 0x30FF), (0x4E00, 0x4E00)]));
        for (from, to) in [
            ("OFL-1.1", "GPL-3.0"),
            ("Test-Regular.ttf", "../x.ttf"),
            ("Test-Regular.ttf", "Test-Regular.otf"),
            ("Test-Regular.ttf", "sub/x.ttf"),
            ("weight = 400", "weight = 0"),
            ("3040-30FF", "30FF-3040"),
            ("org.atrium.font.test", "Org.Atrium"),
            ("kind = \"font\"", "kind = \"app\""),
        ] {
            assert!(FontManifest::from_str(&sample().replace(from, to)).is_err(), "{to}");
        }
        assert!(FontManifest::from_str(&(sample() + "evil = 1\n")).is_err());
    }
}
