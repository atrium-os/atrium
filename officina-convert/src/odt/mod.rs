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
