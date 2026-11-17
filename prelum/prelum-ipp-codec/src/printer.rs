//! A `Get-Printer-Attributes` answer turned into checked values for prelumd
//! (printing.md §6.3: "replies become typed values ... checked against a fixed
//! list; free text cut to 200 bytes of printable UTF-8").
//!
//! Keywords and media types that don't look like keywords are dropped, lists
//! are capped, and state reasons are mapped onto a fixed set; nothing a
//! printer says reaches prelumd or the sheet as free-form structure.

use crate::ops::Format;
use crate::{display_text, tag, Message, Value};

/// At most this many entries are kept from any list attribute.
pub const MAX_LIST: usize = 256;
/// Free text (`printer-info`, `printer-location`, ...) is cut to this.
pub const MAX_TEXT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PrinterState {
    Idle,
    Processing,
    Stopped,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Severity {
    Report,
    Warning,
    Error,
}

/// The state reasons the sheet, Forum and Praeco show (§6.6). Anything else a
/// printer reports is `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Reason {
    MediaEmpty,
    MediaLow,
    MediaJam,
    MediaNeeded,
    TonerLow,
    TonerEmpty,
    MarkerSupplyLow,
    MarkerSupplyEmpty,
    DoorOpen,
    CoverOpen,
    InputTrayMissing,
    OutputAreaFull,
    Offline,
    Paused,
    Shutdown,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StateReason {
    pub reason: Reason,
    pub severity: Severity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Marker {
    pub name: String,
    pub color: String,
    /// 0–100, or negative for "unknown" as IPP reports it.
    pub level: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PrinterInfo {
    pub uuid: Option<String>,
    pub make_and_model: String,
    pub info: String,
    pub location: String,
    pub state: Option<PrinterState>,
    pub state_reasons: Vec<StateReason>,
    pub formats: Vec<String>,
    pub pwg_resolutions: Vec<(i32, i32)>,
    /// `printer-resolution-supported`, dots per inch.
    pub printer_resolutions: Vec<(i32, i32)>,
    pub pwg_types: Vec<String>,
    /// `pwg-raster-document-sheet-back`: how the printer wants the back of a
    /// two-sided sheet drawn (`normal`, `flipped`, `rotated`,
    /// `manual-tumble`).
    pub pwg_sheet_back: Option<String>,
    pub urf: Vec<String>,
    pub media_supported: Vec<String>,
    pub media_default: Option<String>,
    pub sides_supported: Vec<String>,
    pub color_modes: Vec<String>,
    pub qualities: Vec<i32>,
    pub operations: Vec<i32>,
    pub identify_actions: Vec<String>,
    /// Longest PIN the printer takes for `job-password`, if it supports one.
    pub job_password_max: Option<i32>,
    pub markers: Vec<Marker>,
    /// `printer-uri-supported` paired with `uri-security-supported`.
    pub uris: Vec<(String, String)>,
}

impl Default for PrinterState {
    fn default() -> Self {
        PrinterState::Unknown
    }
}

fn is_keyword(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'))
}

fn is_mime(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.bytes().all(|b| b.is_ascii_graphic() && b != b'"')
}

fn is_uri(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1023 && s.bytes().all(|b| b.is_ascii_graphic())
}

fn strings<'a>(m: &'a Message, name: &str) -> impl Iterator<Item = &'a str> {
    m.attribute(tag::PRINTER, name).into_iter().flat_map(|a| a.values.iter()).filter_map(Value::as_str)
}

fn keywords(m: &Message, name: &str) -> Vec<String> {
    strings(m, name).filter(|s| is_keyword(s)).take(MAX_LIST).map(str::to_string).collect()
}

fn ints(m: &Message, name: &str) -> Vec<i32> {
    m.attribute(tag::PRINTER, name).into_iter().flat_map(|a| a.values.iter()).filter_map(Value::as_int).take(MAX_LIST).collect()
}

fn text(m: &Message, name: &str) -> String {
    strings(m, name).next().map(|s| display_text(s, MAX_TEXT)).unwrap_or_default()
}

fn resolutions(m: &Message, name: &str) -> Vec<(i32, i32)> {
    m.attribute(tag::PRINTER, name)
        .into_iter()
        .flat_map(|a| a.values.iter())
        .filter_map(|v| match v {
            Value::Resolution { x, y, units: 3 } if (50..=4800).contains(x) && (50..=4800).contains(y) => Some((*x, *y)),
            _ => None,
        })
        .take(MAX_LIST)
        .collect()
}

pub fn reason(keyword: &str) -> Option<StateReason> {
    if !is_keyword(keyword) {
        return None;
    }
    let (base, severity) = if let Some(b) = keyword.strip_suffix("-error") {
        (b, Severity::Error)
    } else if let Some(b) = keyword.strip_suffix("-warning") {
        (b, Severity::Warning)
    } else if let Some(b) = keyword.strip_suffix("-report") {
        (b, Severity::Report)
    } else {
        (keyword, Severity::Error)
    };
    let reason = match base {
        "none" => return None,
        "media-empty" => Reason::MediaEmpty,
        "media-low" => Reason::MediaLow,
        "media-jam" => Reason::MediaJam,
        "media-needed" => Reason::MediaNeeded,
        "toner-low" => Reason::TonerLow,
        "toner-empty" => Reason::TonerEmpty,
        "marker-supply-low" => Reason::MarkerSupplyLow,
        "marker-supply-empty" => Reason::MarkerSupplyEmpty,
        "door-open" => Reason::DoorOpen,
        "cover-open" => Reason::CoverOpen,
        "input-tray-missing" => Reason::InputTrayMissing,
        "output-area-full" => Reason::OutputAreaFull,
        "offline" => Reason::Offline,
        "paused" | "moving-to-paused" => Reason::Paused,
        "shutdown" => Reason::Shutdown,
        _ => Reason::Other,
    };
    Some(StateReason { reason, severity })
}

impl PrinterInfo {
    pub fn from_response(m: &Message) -> PrinterInfo {
        let state = ints(m, "printer-state").first().map(|s| match s {
            3 => PrinterState::Idle,
            4 => PrinterState::Processing,
            5 => PrinterState::Stopped,
            _ => PrinterState::Unknown,
        });
        let mut state_reasons: Vec<StateReason> = Vec::new();
        for k in strings(m, "printer-state-reasons").take(MAX_LIST) {
            if let Some(r) = reason(k) {
                if !state_reasons.contains(&r) {
                    state_reasons.push(r);
                }
            }
        }
        let uuid = strings(m, "printer-uuid")
            .find_map(|u| {
                let h = u.strip_prefix("urn:uuid:")?;
                (h.len() == 36 && h.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then(|| h.to_ascii_lowercase())
            });
        let pwg_resolutions = resolutions(m, "pwg-raster-document-resolution-supported");
        let names: Vec<String> = strings(m, "marker-names").take(32).map(|s| display_text(s, 64)).collect();
        let colors: Vec<String> = strings(m, "marker-colors").take(32).map(|s| display_text(s, 64)).collect();
        let levels = ints(m, "marker-levels");
        let markers = names
            .iter()
            .enumerate()
            .map(|(i, n)| Marker {
                name: n.clone(),
                color: colors.get(i).cloned().unwrap_or_default(),
                level: levels.get(i).copied().map(|l| l.clamp(-3, 100)).unwrap_or(-2),
            })
            .collect();
        let uri_list: Vec<String> = strings(m, "printer-uri-supported").filter(|u| is_uri(u)).take(8).map(str::to_string).collect();
        let sec: Vec<String> = keywords(m, "uri-security-supported");
        let uris = uri_list.into_iter().enumerate().map(|(i, u)| (u, sec.get(i).cloned().unwrap_or_else(|| "none".into()))).collect();
        PrinterInfo {
            uuid,
            make_and_model: text(m, "printer-make-and-model"),
            info: text(m, "printer-info"),
            location: text(m, "printer-location"),
            state,
            state_reasons,
            formats: strings(m, "document-format-supported").filter(|s| is_mime(s)).take(MAX_LIST).map(|s| s.to_ascii_lowercase()).collect(),
            pwg_resolutions,
            printer_resolutions: resolutions(m, "printer-resolution-supported"),
            pwg_types: keywords(m, "pwg-raster-document-type-supported"),
            pwg_sheet_back: keywords(m, "pwg-raster-document-sheet-back").into_iter().next(),
            urf: strings(m, "urf-supported").filter(|s| s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).take(MAX_LIST).map(str::to_string).collect(),
            media_supported: keywords(m, "media-supported"),
            media_default: keywords(m, "media-default").into_iter().next(),
            sides_supported: keywords(m, "sides-supported"),
            color_modes: keywords(m, "print-color-mode-supported"),
            qualities: ints(m, "print-quality-supported").into_iter().filter(|q| (3..=5).contains(q)).collect(),
            operations: ints(m, "operations-supported"),
            identify_actions: keywords(m, "identify-actions-supported"),
            job_password_max: ints(m, "job-password-supported").first().copied().filter(|n| (1..=255).contains(n)),
            markers,
            uris,
        }
    }

    pub fn supports(&self, op: u16) -> bool {
        self.operations.contains(&(op as i32))
    }

    /// The format to send (§6.2): rewritten PDF, then PWG Raster, then URF.
    /// `raster_only` is the per-printer "Send pages as images" switch or a
    /// shipped quirk.
    pub fn choose_format(&self, raster_only: bool) -> Option<Format> {
        let has = |m: &str| self.formats.iter().any(|f| f == m);
        if !raster_only && has("application/pdf") {
            return Some(Format::Pdf);
        }
        if has("image/pwg-raster") && !self.pwg_resolutions.is_empty() {
            return Some(Format::PwgRaster);
        }
        if has("image/urf") && self.urf_resolutions().next().is_some() {
            return Some(Format::Urf);
        }
        None
    }

    /// Resolutions from `urf-supported`'s `RS` entry (`RS300-600`).
    pub fn urf_resolutions(&self) -> impl Iterator<Item = i32> + '_ {
        self.urf
            .iter()
            .filter_map(|e| e.strip_prefix("RS"))
            .flat_map(|r| r.split('-'))
            .filter_map(|n| n.parse::<i32>().ok())
            .filter(|n| (50..=4800).contains(n))
    }

    /// Whether `printer-resolution` may name this resolution in a ticket. A
    /// raster stream states its own resolution in each page header; the
    /// ticket attribute is sent only when the printer lists it.
    pub fn ticket_resolution(&self, dpi: (i32, i32)) -> Option<(i32, i32)> {
        self.printer_resolutions.contains(&dpi).then_some(dpi)
    }

    /// The back-side rule for URF, from `urf-supported`'s `DM` entry
    /// (DM1 normal, DM2 flipped, DM3 rotated, DM4 manual tumble, as other
    /// AirPrint clients read it; to be confirmed on a real printer, R8).
    pub fn urf_sheet_back(&self) -> Option<&'static str> {
        self.urf.iter().find_map(|e| match e.as_str() {
            "DM1" => Some("normal"),
            "DM2" => Some("flipped"),
            "DM3" => Some("rotated"),
            "DM4" => Some("manual-tumble"),
            _ => None,
        })
    }

    pub fn tls(&self) -> bool {
        self.uris.iter().any(|(u, s)| u.starts_with("ipps://") && s == "tls")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attribute, Group, Message};

    fn printer(attrs: Vec<Attribute>) -> Message {
        Message { version: (2, 0), code: 0, request_id: 1, groups: vec![Group { tag: tag::PRINTER, attributes: attrs }] }
    }
    fn kws(name: &str, v: &[&str]) -> Attribute {
        Attribute::many(name, v.iter().map(|s| Value::Keyword((*s).into())).collect())
    }
    fn mimes(v: &[&str]) -> Attribute {
        Attribute::many("document-format-supported", v.iter().map(|s| Value::MimeMediaType((*s).into())).collect())
    }

    #[test]
    fn format_order() {
        let base = vec![
            mimes(&["application/octet-stream", "image/urf", "image/pwg-raster", "application/pdf"]),
            Attribute::new("pwg-raster-document-resolution-supported", Value::Resolution { x: 600, y: 600, units: 3 }),
            kws("urf-supported", &["V1.4", "CP1", "W8", "SRGB24", "RS300-600"]),
        ];
        let p = PrinterInfo::from_response(&printer(base.clone()));
        assert_eq!(p.choose_format(false), Some(Format::Pdf));
        assert_eq!(p.choose_format(true), Some(Format::PwgRaster));
        assert_eq!(p.urf_resolutions().collect::<Vec<_>>(), [300, 600]);
        let p = PrinterInfo::from_response(&printer(vec![mimes(&["image/urf"]), kws("urf-supported", &["W8", "RS600"])]));
        assert_eq!(p.choose_format(false), Some(Format::Urf));
        let p = PrinterInfo::from_response(&printer(vec![mimes(&["application/postscript"])]));
        assert_eq!(p.choose_format(false), None);
    }

    #[test]
    fn hostile_values_are_dropped_or_cut() {
        let long = "x".repeat(5000);
        let p = PrinterInfo::from_response(&printer(vec![
            Attribute::new("printer-info", Value::Text(format!("Office\u{202E}{long}"))),
            kws("media-supported", &["iso_a4_210x297mm", "BAD KEYWORD", "na_letter_8.5x11in"]),
            Attribute::new("printer-uuid", Value::Uri("urn:uuid:not-a-uuid".into())),
            kws("printer-state-reasons", &["media-empty-error", "toner-low-report", "vendor-thing", "none"]),
            Attribute::new("printer-state", Value::Enum(4)),
        ]));
        assert!(p.info.len() <= MAX_TEXT && !p.info.contains('\u{202E}'));
        assert_eq!(p.media_supported, ["iso_a4_210x297mm", "na_letter_8.5x11in"]);
        assert_eq!(p.uuid, None);
        assert_eq!(p.state, Some(PrinterState::Processing));
        assert_eq!(
            p.state_reasons,
            [
                StateReason { reason: Reason::MediaEmpty, severity: Severity::Error },
                StateReason { reason: Reason::TonerLow, severity: Severity::Report },
                StateReason { reason: Reason::Other, severity: Severity::Error },
            ]
        );
    }

    #[test]
    fn uuid_and_tls() {
        let p = PrinterInfo::from_response(&printer(vec![
            Attribute::new("printer-uuid", Value::Uri("urn:uuid:4509A320-00A0-008F-00B6-00A0D1D2E3F4".into())),
            Attribute::many("printer-uri-supported", vec![Value::Uri("ipp://p/ipp/print".into()), Value::Uri("ipps://p/ipp/print".into())]),
            kws("uri-security-supported", &["none", "tls"]),
        ]));
        assert_eq!(p.uuid.as_deref(), Some("4509a320-00a0-008f-00b6-00a0d1d2e3f4"));
        assert!(p.tls());
    }
}
