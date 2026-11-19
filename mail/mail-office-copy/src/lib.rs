//! Only this wire format and the pixel-to-PDF writer are linked into Mail.
//! The worker cannot supply PDF objects, annotations, metadata or programs.
#[derive(atrium_message::Message)]
pub enum OfficeFormat {
    Docx,
    Odt,
    Xlsx,
    Ods,
}
#[derive(atrium_message::Message)]
#[message(version = 2)]
pub struct RenderOffice {
    pub format: OfficeFormat,
}

#[derive(atrium_message::Message)]
pub struct PagePixels {
    #[message(min = 1, max = 48000)]
    pub width_twips: u32,
    #[message(min = 1, max = 48000)]
    pub height_twips: u32,
    #[message(max_len = 33554432)]
    pub samples: Vec<u8>,
}

#[derive(atrium_message::Message)]
#[message(version = 1)]
pub struct OfficeCopy {
    #[message(max_len = 64)]
    pub pages: Vec<PagePixels>,
}

impl OfficeCopy {
    /// Validate every page from the untrusted worker before returning any file.
    pub fn into_pdf(self) -> Result<Vec<u8>, String> {
        if self.pages.is_empty() || self.pages.len() > 64 {
            return Err("invalid page count".into());
        }
        let mut pdf = folium_syntax::write::ImagePages::new().map_err(|e| e.to_string())?;
        let mut total = 0u64;
        for page in self.pages {
            let (pw, ph) = (
                page.width_twips.div_ceil(10),
                page.height_twips.div_ceil(10),
            );
            let pixels = u64::from(pw) * u64::from(ph);
            if pixels == 0 || pixels > 8_000_000 || total + pixels > 48_000_000 {
                return Err("page image limit".into());
            }
            total += pixels;
            let len = pixels as usize * 3;
            let rgb = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&page.samples, len)
                .map_err(|_| "invalid page pixels")?;
            if rgb.len() != len {
                return Err("incomplete page pixels".into());
            }
            pdf.page(
                f64::from(page.width_twips) / 20.,
                f64::from(page.height_twips) / 20.,
                pw,
                ph,
                &rgb,
            )
            .map_err(|e| e.to_string())?;
        }
        let bytes = pdf.finish().map_err(|e| e.to_string())?;
        if bytes.len() > 32 << 20 {
            return Err("PDF size limit".into());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_last_bitmap_refuses_the_entire_result() {
        let page = |rgb: &[u8]| PagePixels {
            width_twips: 10,
            height_twips: 10,
            samples: miniz_oxide::deflate::compress_to_vec_zlib(rgb, 1),
        };
        for last in [&[1, 2][..], &[1, 2, 3, 4][..]] {
            assert!(OfficeCopy {
                pages: vec![page(&[255, 255, 255]), page(last)]
            }
            .into_pdf()
            .is_err());
        }
    }
}
