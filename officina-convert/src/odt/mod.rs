//! OpenDocument Text (`.odt`): reader and writer for officina-text.

pub mod read;
pub mod write;

pub use read::{read, OdtError};
pub use write::write;

#[cfg(test)]
mod tests {
    use crate::docx::tests::{same, sample};

    #[test]
    fn round_trip() {
        let d = sample();
        let bytes = super::write(&d, Vec::new()).unwrap();
        let (r, loaded) = super::read(&bytes).unwrap();
        same(&d, &r);
        assert_eq!(loaded.pictures, 1);
        assert!(loaded.report.is_empty(), "{:?}", loaded.report.lines());
    }

    /// A right-to-left paragraph and a left-to-right one say so after a
    /// round trip.
    #[test]
    fn paragraph_direction_round_trips() {
        let mut d = officina_text::model::TextDoc::new();
        let a = d.push_para("שלום", 0, vec![]);
        let b = d.push_para("hello", 0, vec![]);
        d.blocks.update(a, |x| x.as_para_mut().unwrap().props.rtl = Some(true));
        d.blocks.update(b, |x| x.as_para_mut().unwrap().props.rtl = Some(false));
        let (r, _) = super::read(&super::write(&d, Vec::new()).unwrap()).unwrap();
        let dirs: Vec<Option<bool>> = (0..r.blocks.len()).filter_map(|i| r.para(i)).filter(|p| !p.text.is_empty()).map(|p| p.props.rtl).collect();
        assert_eq!(dirs, [Some(true), Some(false)]);
    }

    #[test]
    fn headers_keep_their_paragraphs_pictures_and_page_numbers() {
        let d = crate::docx::tests::margin_sample();
        let (r, _) = super::read(&super::write(&d, Vec::new()).unwrap()).unwrap();
        crate::docx::tests::margin_same(&d, &r);
    }

    #[test]
    fn hostile_packages_fail_cleanly() {
        let bytes = super::write(&sample(), Vec::new()).unwrap();
        let mut x: u64 = 0x0d7;
        for _ in 0..2000 {
            let mut b = bytes.clone();
            for _ in 0..1 + (x % 8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % b.len();
                b[i] = (x >> 32) as u8;
            }
            if x % 5 == 0 {
                b.truncate((x as usize) % b.len());
            }
            let _ = super::read(&b);
        }
    }
}
