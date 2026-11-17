//! The requests Prelum sends (printing.md §6.3), built with the attributes in
//! the order RFC 8011 §4.1.4 wants: charset, natural language, target.
//!
//! Page ranges, scaling, orientation and pages per sheet are not sent:
//! `prelum-render` applies them, so every printer prints the same thing
//! (§6.1). What goes in the ticket is what only the printer can do.

use crate::{tag, Attribute, Group, Message, Value};

pub mod op {
    pub const PRINT_JOB: u16 = 0x0002;
    pub const VALIDATE_JOB: u16 = 0x0004;
    pub const CREATE_JOB: u16 = 0x0005;
    pub const SEND_DOCUMENT: u16 = 0x0006;
    pub const CANCEL_JOB: u16 = 0x0008;
    pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;
    pub const GET_PRINTER_ATTRIBUTES: u16 = 0x000B;
    pub const IDENTIFY_PRINTER: u16 = 0x003C;
}

/// Document formats Prelum sends (§6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Format {
    Pdf,
    PwgRaster,
    Urf,
}

impl Format {
    pub fn mime(self) -> &'static str {
        match self {
            Format::Pdf => "application/pdf",
            Format::PwgRaster => "image/pwg-raster",
            Format::Urf => "image/urf",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Sides {
    OneSided,
    TwoSidedLongEdge,
    TwoSidedShortEdge,
}

impl Sides {
    pub fn keyword(self) -> &'static str {
        match self {
            Sides::OneSided => "one-sided",
            Sides::TwoSidedLongEdge => "two-sided-long-edge",
            Sides::TwoSidedShortEdge => "two-sided-short-edge",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColorMode {
    Color,
    Monochrome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Quality {
    Draft = 3,
    Normal = 4,
    High = 5,
}

/// What the person chose in the sheet that the printer itself carries out.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Ticket {
    pub copies: u32,
    pub sides: Sides,
    pub color: ColorMode,
    /// A PWG media keyword such as `iso_a4_210x297mm`, from the printer's
    /// `media-supported`.
    pub media: Option<String>,
    pub quality: Option<Quality>,
    /// Required for raster: the resolution the pages were drawn at.
    pub resolution: Option<(i32, i32)>,
}

impl Default for Ticket {
    fn default() -> Self {
        Ticket { copies: 1, sides: Sides::OneSided, color: ColorMode::Color, media: None, quality: None, resolution: None }
    }
}

/// Who and what the job says about us (§6.3, decision 14).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JobIdentity {
    /// `atrium` unless the printer needs a real name.
    pub user: String,
    /// The document title, or "Document" when titles are off for this printer.
    pub job_name: String,
    /// A PIN for secure release, from Arca, per job (§8).
    pub job_password: Option<Vec<u8>>,
}

fn base(op: u16, request_id: u32, printer_uri: &str) -> (Message, Vec<Attribute>) {
    let msg = Message { version: (2, 0), code: op, request_id, groups: Vec::new() };
    let attrs = vec![
        Attribute::new("attributes-charset", Value::Charset("utf-8".into())),
        Attribute::new("attributes-natural-language", Value::NaturalLanguage("en".into())),
        Attribute::new("printer-uri", Value::Uri(printer_uri.into())),
    ];
    (msg, attrs)
}

fn finish(mut msg: Message, op_attrs: Vec<Attribute>, job_attrs: Vec<Attribute>) -> Message {
    msg.groups.push(Group { tag: tag::OPERATION, attributes: op_attrs });
    if !job_attrs.is_empty() {
        msg.groups.push(Group { tag: tag::JOB, attributes: job_attrs });
    }
    msg
}

fn user(attrs: &mut Vec<Attribute>, user: &str) {
    attrs.push(Attribute::new("requesting-user-name", Value::Name(user.into())));
}

fn ticket_attrs(t: &Ticket, format: Format) -> Vec<Attribute> {
    let mut a = Vec::new();
    if t.copies > 1 {
        a.push(Attribute::new("copies", Value::Integer(t.copies.min(i32::MAX as u32) as i32)));
        if format == Format::Pdf {
            a.push(Attribute::new("multiple-document-handling", Value::Keyword("separate-documents-collated-copies".into())));
        }
    }
    a.push(Attribute::new("sides", Value::Keyword(t.sides.keyword().into())));
    let cm = match t.color {
        ColorMode::Color => "color",
        ColorMode::Monochrome => "monochrome",
    };
    a.push(Attribute::new("print-color-mode", Value::Keyword(cm.into())));
    if let Some(m) = &t.media {
        a.push(Attribute::new("media", Value::Keyword(m.clone())));
    }
    if let Some(q) = t.quality {
        a.push(Attribute::new("print-quality", Value::Enum(q as i32)));
    }
    if let Some((x, y)) = t.resolution {
        a.push(Attribute::new("printer-resolution", Value::Resolution { x, y, units: 3 }));
    }
    a
}

fn job_op_attrs(attrs: &mut Vec<Attribute>, id: &JobIdentity) {
    user(attrs, &id.user);
    attrs.push(Attribute::new("job-name", Value::Name(id.job_name.clone())));
    if let Some(p) = &id.job_password {
        attrs.push(Attribute::new("job-password", Value::OctetString(p.clone())));
        attrs.push(Attribute::new("job-password-encryption", Value::Keyword("none".into())));
    }
}

pub fn get_printer_attributes(request_id: u32, printer_uri: &str, requested: &[&str]) -> Message {
    let (m, mut a) = base(op::GET_PRINTER_ATTRIBUTES, request_id, printer_uri);
    user(&mut a, "atrium");
    if !requested.is_empty() {
        a.push(Attribute::many("requested-attributes", requested.iter().map(|k| Value::Keyword((*k).into())).collect()));
    }
    finish(m, a, Vec::new())
}

pub fn validate_job(request_id: u32, printer_uri: &str, id: &JobIdentity, format: Format, t: &Ticket) -> Message {
    let (m, mut a) = base(op::VALIDATE_JOB, request_id, printer_uri);
    job_op_attrs(&mut a, id);
    a.push(Attribute::new("document-format", Value::MimeMediaType(format.mime().into())));
    finish(m, a, ticket_attrs(t, format))
}

pub fn create_job(request_id: u32, printer_uri: &str, id: &JobIdentity, format: Format, t: &Ticket) -> Message {
    let (m, mut a) = base(op::CREATE_JOB, request_id, printer_uri);
    job_op_attrs(&mut a, id);
    finish(m, a, ticket_attrs(t, format))
}

/// Followed on the wire by the document bytes.
pub fn send_document(request_id: u32, printer_uri: &str, job_id: i32, user_name: &str, format: Format, last: bool) -> Message {
    let (m, mut a) = base(op::SEND_DOCUMENT, request_id, printer_uri);
    a.push(Attribute::new("job-id", Value::Integer(job_id)));
    user(&mut a, user_name);
    a.push(Attribute::new("document-format", Value::MimeMediaType(format.mime().into())));
    a.push(Attribute::new("last-document", Value::Boolean(last)));
    finish(m, a, Vec::new())
}

/// For printers without Create-Job; followed by the document bytes.
pub fn print_job(request_id: u32, printer_uri: &str, id: &JobIdentity, format: Format, t: &Ticket) -> Message {
    let (m, mut a) = base(op::PRINT_JOB, request_id, printer_uri);
    job_op_attrs(&mut a, id);
    a.push(Attribute::new("document-format", Value::MimeMediaType(format.mime().into())));
    finish(m, a, ticket_attrs(t, format))
}

pub fn get_job_attributes(request_id: u32, printer_uri: &str, job_id: i32, user_name: &str) -> Message {
    let (m, mut a) = base(op::GET_JOB_ATTRIBUTES, request_id, printer_uri);
    a.push(Attribute::new("job-id", Value::Integer(job_id)));
    user(&mut a, user_name);
    a.push(Attribute::many(
        "requested-attributes",
        ["job-state", "job-state-reasons", "job-impressions-completed", "job-pages-completed"]
            .iter()
            .map(|k| Value::Keyword((*k).into()))
            .collect(),
    ));
    finish(m, a, Vec::new())
}

pub fn cancel_job(request_id: u32, printer_uri: &str, job_id: i32, user_name: &str) -> Message {
    let (m, mut a) = base(op::CANCEL_JOB, request_id, printer_uri);
    a.push(Attribute::new("job-id", Value::Integer(job_id)));
    user(&mut a, user_name);
    finish(m, a, Vec::new())
}

/// `actions` from the printer's `identify-actions-supported` (`flash`,
/// `sound`, `display`, ...); empty lets the printer choose.
pub fn identify_printer(request_id: u32, printer_uri: &str, actions: &[&str]) -> Message {
    let (m, mut a) = base(op::IDENTIFY_PRINTER, request_id, printer_uri);
    user(&mut a, "atrium");
    if !actions.is_empty() {
        a.push(Attribute::many("identify-actions", actions.iter().map(|k| Value::Keyword((*k).into())).collect()));
    }
    finish(m, a, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    #[test]
    fn create_job_shape() {
        let id = JobIdentity { user: "atrium".into(), job_name: "Report".into(), job_password: Some(b"1234".to_vec()) };
        let t = Ticket { copies: 2, sides: Sides::TwoSidedLongEdge, media: Some("iso_a4_210x297mm".into()), ..Ticket::default() };
        let m = create_job(3, "ipps://p.local:631/ipp/print", &id, Format::Pdf, &t);
        let (d, _) = Message::decode(&m.encode(), &Limits::default()).unwrap();
        let op = d.group(tag::OPERATION).unwrap();
        let names: Vec<_> = op.attributes.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(&names[..3], ["attributes-charset", "attributes-natural-language", "printer-uri"]);
        assert_eq!(d.attribute(tag::OPERATION, "job-password").unwrap().first(), Some(&Value::OctetString(b"1234".to_vec())));
        assert_eq!(d.attribute(tag::JOB, "sides").unwrap().first().unwrap().as_str(), Some("two-sided-long-edge"));
        assert_eq!(d.attribute(tag::JOB, "copies").unwrap().first().unwrap().as_int(), Some(2));
    }

    #[test]
    fn raster_ticket_carries_resolution() {
        let id = JobIdentity { user: "atrium".into(), job_name: "Document".into(), job_password: None };
        let t = Ticket { resolution: Some((600, 600)), ..Ticket::default() };
        let m = print_job(1, "ipp://p/ipp/print", &id, Format::PwgRaster, &t);
        assert_eq!(
            m.attribute(tag::JOB, "printer-resolution").unwrap().first(),
            Some(&Value::Resolution { x: 600, y: 600, units: 3 })
        );
        assert_eq!(m.attribute(tag::OPERATION, "document-format").unwrap().first().unwrap().as_str(), Some("image/pwg-raster"));
    }
}
