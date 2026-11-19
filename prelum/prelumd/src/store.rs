//! Saved printers and history, files per person (printing.md §6.4, §6.5):
//! `<state>/<uid>/printers.toml` and `history.toml`, 0600 in a 0700
//! directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use prelum_ipp_codec::printer::PrinterInfo;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub id: u32,
    pub name: String,
    /// As the printer names itself: `ipps://host:631/ipp/print`.
    pub uri: String,
    /// The address it was resolved to when added: the only one contacted.
    pub addr: String,
    pub tls: bool,
    /// Hex SHA-256 of its TLS key (IPPS).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    /// The network it was added on (§7.3).
    pub network: String,
    /// "Send pages as images" (§6.2).
    #[serde(default)]
    pub raster_only: bool,
    /// Send "Document" instead of titles (§6.3).
    #[serde(default)]
    pub titles_off: bool,
    #[serde(default)]
    pub jobs_sent: u32,
    /// The persona it was added from ("" for Personal), §9.
    #[serde(default)]
    pub persona: String,
    /// Every persona's (§9).
    #[serde(default)]
    pub shared: bool,
    /// What it said when last asked: kept so the sheet has media and sides
    /// before the printer answers again.
    #[serde(default)]
    pub info: PrinterInfo,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mute_printed_notice: bool,
    #[serde(default)]
    pub printer: Vec<Saved>,
}

pub fn path(state: &Path, uid: u32) -> PathBuf {
    state.join(uid.to_string()).join("printers.toml")
}

/// The jobs that ended (§6.5), oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub job: Vec<prelum_proto::print::Ended>,
}

pub fn history_path(state: &Path, uid: u32) -> PathBuf {
    state.join(uid.to_string()).join("history.toml")
}

pub fn load_history(state: &Path, uid: u32) -> History {
    let p = history_path(state, uid);
    match std::fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            eprintln!("prelumd: {}: {e}", p.display());
            History::default()
        }),
        Err(_) => History::default(),
    }
}

pub fn save_history(state: &Path, uid: u32, h: &History) -> std::io::Result<()> {
    write(&history_path(state, uid), h)
}

pub fn load(state: &Path, uid: u32) -> File {
    match std::fs::read_to_string(path(state, uid)) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            eprintln!("prelumd: {}: {e}", path(state, uid).display());
            File::default()
        }),
        Err(_) => File::default(),
    }
}

/// Written to a new file and renamed over the old one.
pub fn save(state: &Path, uid: u32, f: &File) -> std::io::Result<()> {
    write(&path(state, uid), f)
}

fn write<T: Serialize>(p: &Path, v: &T) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = p.parent().unwrap();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let text = toml::to_string(v).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let tmp = dir.join(format!(".{}.new", p.file_name().and_then(|n| n.to_str()).unwrap_or("file")));
    let mut o = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    o.write_all(text.as_bytes())?;
    o.sync_all()?;
    std::fs::rename(&tmp, p)
}

pub fn hex(k: &[u8; 32]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prelum_ipp_codec::printer::{Marker, PrinterState, Reason, Severity, StateReason};

    #[test]
    fn round_trip() {
        let d = tempfile::tempdir().unwrap();
        let info = PrinterInfo {
            uuid: Some("abc".into()),
            make_and_model: "Test".into(),
            state: Some(PrinterState::Idle),
            state_reasons: vec![StateReason { reason: Reason::MediaLow, severity: Severity::Warning }],
            pwg_resolutions: vec![(300, 300), (600, 600)],
            markers: vec![Marker { name: "Black".into(), color: "#000000".into(), level: 40 }],
            uris: vec![("ipps://x/ipp/print".into(), "tls".into())],
            ..Default::default()
        };
        let f = File {
            mute_printed_notice: true,
            printer: vec![Saved {
                id: 1,
                name: "Office".into(),
                uri: "ipps://10.0.0.5:631/ipp/print".into(),
                addr: "10.0.0.5:631".into(),
                tls: true,
                key: Some(hex(&[7; 32])),
                uuid: Some("abc".into()),
                network: "local".into(),
                raster_only: false,
                titles_off: false,
                jobs_sent: 0,
                persona: "Work".into(),
                shared: true,
                info,
            }],
        };
        save(d.path(), 1000, &f).unwrap();
        assert_eq!(load(d.path(), 1000), f);
        let h = History {
            job: vec![prelum_proto::print::Ended {
                job: 3,
                title: "Q3".into(),
                printer: "Office".into(),
                sheets: 2,
                at: 1_700_000_000,
                result: prelum_proto::print::Phase::Failed,
                message: "the printer stopped the job".into(),
                persona: String::new(),
            }],
        };
        save_history(d.path(), 1000, &h).unwrap();
        assert_eq!(load_history(d.path(), 1000), h);
        assert_eq!(unhex(&hex(&[9; 32])), Some([9; 32]));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(path(d.path(), 1000)).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
