use mail_office_copy::{OfficeCopy, RenderOffice};
use std::{fs::File, io::Read};
mod paint;
mod profile;
mod sheets;

type Error = Box<dyn std::error::Error>;
fn convert(odt: bool, bytes: &[u8]) -> Result<OfficeCopy, Error> {
    let expected = profile::check(odt, bytes)?;
    let (doc, loaded) = if odt {
        officina_convert::odt::read(bytes)?
    } else {
        officina_convert::docx::read(bytes)?
    };
    if !loaded.report.is_empty() {
        return Err("Officina reported incomplete import".into());
    }
    paint::pages(&doc, &expected)
}

fn handle(req: RenderOffice, file: Option<File>) -> Result<OfficeCopy, String> {
    const LIMIT: u64 = 8 << 20;
    let mut bytes = Vec::new();
    file.ok_or("missing attachment")?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("attachment too large".into());
    }
    use mail_office_copy::OfficeFormat::*;
    match req.format {
        Docx => convert(false, &bytes),
        Odt => convert(true, &bytes),
        Xlsx => sheets::convert(false, &bytes),
        Ods => sheets::convert(true, &bytes),
    }
    .map_err(|e| e.to_string())
}
atrium_worker::main!(handle);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_both_pages_of_each_format_as_distinct_images() {
        for (odt, input) in [
            (
                false,
                include_bytes!("../tests/fixtures/complete.docx").as_slice(),
            ),
            (
                true,
                include_bytes!("../tests/fixtures/complete.odt").as_slice(),
            ),
        ] {
            let copy = convert(odt, input).unwrap();
            assert_eq!(copy.pages.len(), 2);
            assert_ne!(copy.pages[0].samples, copy.pages[1].samples);
            for page in &copy.pages {
                let rgb = miniz_oxide::inflate::decompress_to_vec_zlib(&page.samples).unwrap();
                assert!(
                    rgb.chunks_exact(3)
                        .filter(|p| p.iter().all(|v| *v < 128))
                        .count()
                        > 200
                );
            }
            let pdf = copy.into_pdf().unwrap();
            let syntax = String::from_utf8_lossy(&pdf);
            assert_eq!(syntax.matches("/Subtype /Image").count(), 2);
            assert!(!syntax.contains("/Font"));
            assert!(!syntax.contains("/Annots"));
        }
    }
    #[test]
    fn accepts_plain_documents_saved_by_officina_with_default_styles() {
        let mut doc = officina_text::model::TextDoc::new();
        doc.push_para("An ordinary letter", 0, vec![]);
        for odt in [true, false] {
            let bytes = if odt {
                officina_convert::odt::write(&doc, Vec::new()).unwrap()
            } else {
                officina_convert::docx::write(&doc, Vec::new()).unwrap()
            };
            assert_eq!(convert(odt, &bytes).unwrap().pages.len(), 1);
        }
    }
    #[test]
    fn final_unsupported_object_or_wrong_format_refuses_the_entire_copy() {
        for (odt, input) in [
            (
                false,
                include_bytes!("../tests/fixtures/incomplete.docx").as_slice(),
            ),
            (
                true,
                include_bytes!("../tests/fixtures/incomplete.odt").as_slice(),
            ),
        ] {
            assert!(convert(odt, input).is_err());
            assert!(convert(!odt, input).is_err());
        }
    }
}
