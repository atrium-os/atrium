//! A fresh viewer per export keeps its resource table private.
use limen::client::Parent;
use limen::doc_viewer::{DocKind, ToChild, ToParent, DOC_MAX};
use limen::proto::ParentEvent;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use limen::pdf_viewer as folium;
use mail_engine::attachments::{CopyKind, ExportSource};
use mail_office_copy::OfficeFormat;

pub fn render(source: ExportSource, token: Option<[u8; 16]>) -> Result<OwnedFd, String> {
    if let ExportSource::Office { kind, bytes } = source {
        let format = match kind {
            CopyKind::Odt => OfficeFormat::Odt,
            CopyKind::Docx => OfficeFormat::Docx,
            CopyKind::Xlsx => OfficeFormat::Xlsx,
            CopyKind::Ods => OfficeFormat::Ods,
            _ => return Err("Unsupported Office format".into()),
        };
        let input =
            limen::fdpass::sealed_file("office-input", &bytes).map_err(|e| e.to_string())?;
        let workers = atrium_worker::Workers::from_root(std::path::Path::new("/"), false)
            .map_err(|e| e.to_string())?;
        let copy: mail_office_copy::OfficeCopy = workers
            .max_answer((32 << 20) + 4096)
            .call(
                "office-copy",
                &mail_office_copy::RenderOffice { format },
                Some(input.into()),
            )
            .map_err(|e| format!("Officina could not render the whole document: {e}"))?;
        let pdf = copy.into_pdf()?;
        return limen::fdpass::sealed_file("sanitized.pdf", &pdf).map_err(|e| e.to_string());
    }
    let token = token.ok_or("Document viewer unavailable")?;
    let explain = |e| format!("document viewer: {e:?}");
    let mut parent = Parent::connect().map_err(explain)?;
    let visual = matches!(source, ExportSource::Pdf(_));
    let role = if visual {
        "pdf-sanitizer"
    } else {
        "doc-viewer"
    };
    let id = parent.request(role, token).map_err(explain)?;
    // Limen authorizes returned files only after verifying both window
    // owners at bind time. Bind the worker before asking for its PDF.
    parent
        .place(id, Some([0.0, 0.0, 1.0, 1.0]))
        .map_err(explain)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("Document viewer did not attach".into());
        }
        match parent.poll(left).map_err(explain)? {
            Some(ParentEvent::Attached { embed }) if embed == id => break,
            Some(ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. }) | None => {
                return Err("Document viewer unavailable".into())
            }
            _ => {}
        }
    }
    match source {
        ExportSource::Office { .. } => unreachable!(),
        ExportSource::Pdf(bytes) => {
            let fd =
                limen::fdpass::sealed_file("original.pdf", &bytes).map_err(|e| e.to_string())?;
            parent
                .send_fd(id, folium::ToChild::ExportImages.encode(), fd.as_fd())
                .map_err(explain)?;
        }
        ExportSource::Html(doc) => {
            for (resource, bytes) in doc.pictures {
                let fd =
                    limen::fdpass::sealed_file("local-image", &bytes).map_err(|e| e.to_string())?;
                parent
                    .send_fd(id, ToChild::Provide { id: resource }.encode(), fd.as_fd())
                    .map_err(explain)?;
            }
            let fd = limen::fdpass::sealed_file("normalized-html", doc.html.as_bytes())
                .map_err(|e| e.to_string())?;
            let load = ToChild::Load {
                kind: DocKind::Html,
                base: String::new(),
                dark: false,
            };
            parent
                .send_fd(id, load.encode(), fd.as_fd())
                .map_err(explain)?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("PDF conversion timed out".into());
        }
        match parent.poll(left).map_err(explain)? {
            Some(ParentEvent::Message { embed, bytes }) if embed == id && visual => {
                match folium::ToParent::decode(&bytes) {
                    Some(folium::ToParent::PasswordNeeded) => {
                        return Err("Encrypted PDFs cannot be exported".into())
                    }
                    Some(folium::ToParent::Error { .. }) => {
                        return Err("Folium could not render every page completely".into())
                    }
                    _ => {}
                }
            }
            Some(ParentEvent::Message { embed, bytes }) if embed == id => {
                match ToParent::decode(&bytes) {
                    Some(ToParent::Loaded { .. }) => parent
                        .send(id, ToChild::SanitizedPdf.encode())
                        .map_err(explain)?,
                    Some(ToParent::Error { .. }) => {
                        return Err("The viewer could not make a complete PDF".into())
                    }
                    _ => {}
                }
            }
            Some(ParentEvent::MessageFd { embed, bytes }) if embed == id => {
                let complete = if visual {
                    folium::ToParent::decode(&bytes) == Some(folium::ToParent::ImageCopy)
                } else {
                    matches!(
                        ToParent::decode(&bytes),
                        Some(ToParent::Printed { cut: false, .. })
                    )
                };
                if !complete {
                    return Err("Incomplete PDF".into());
                }
                let file = std::fs::File::from(parent.take_fd().ok_or("Missing PDF")?);
                let len = file.metadata().map_err(|e| e.to_string())?.len();
                if len == 0 || len > DOC_MAX {
                    return Err("Invalid PDF size".into());
                }
                // Limen has narrowed the returned descriptor. Reseal the
                // bounded bytes so Portcullis can independently inspect it.
                let mut pdf = Vec::new();
                file.take(len + 1)
                    .read_to_end(&mut pdf)
                    .map_err(|e| e.to_string())?;
                if pdf.len() as u64 != len || !pdf.starts_with(b"%PDF-") {
                    return Err("Invalid PDF".into());
                }
                return limen::fdpass::sealed_file("sanitized.pdf", &pdf)
                    .map_err(|e| e.to_string());
            }
            Some(
                ParentEvent::ChildLost { .. }
                | ParentEvent::Detached { .. }
                | ParentEvent::Dropped { .. },
            )
            | None => return Err("PDF conversion interrupted".into()),
            _ => {}
        }
    }
}
