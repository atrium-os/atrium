//! Which CJK face draws a Han character.
//!
//! One ideograph has different forms in Simplified and Traditional Chinese,
//! Japanese and Korean, and each CJK font package draws one of them (Noto
//! Sans SC, TC, HK, JP, KR). Documents seldom say their language, so the
//! choice comes from the text itself (kana means Japanese, Hangul means
//! Korean), else the person's locale, else install order.

use std::sync::OnceLock;

/// A CJK writing convention, by the family suffix the faces carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Han {
    Sc,
    Tc,
    Hk,
    Jp,
    Kr,
}

impl Han {
    /// The suffix of the families that write it ("Noto Sans JP").
    pub fn suffix(self) -> &'static str {
        match self {
            Han::Sc => "SC",
            Han::Tc => "TC",
            Han::Hk => "HK",
            Han::Jp => "JP",
            Han::Kr => "KR",
        }
    }

    /// From the text around a character: kana says Japanese, Hangul Korean.
    pub fn of_text(text: &str) -> Option<Han> {
        for c in text.chars() {
            match c as u32 {
                0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => return Some(Han::Jp),
                0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => return Some(Han::Kr),
                _ => {}
            }
        }
        None
    }

    /// From a locale name (`ja_JP.UTF-8`, `zh-Hant-TW`, `ko`).
    pub fn of_locale(name: &str) -> Option<Han> {
        let n = name.split(['.', '@']).next().unwrap_or("").replace('-', "_").to_ascii_lowercase();
        let mut parts = n.split('_');
        match parts.next()? {
            "ja" => Some(Han::Jp),
            "ko" => Some(Han::Kr),
            "zh" | "yue" => {
                let rest: Vec<&str> = parts.collect();
                if rest.contains(&"hk") || rest.contains(&"mo") {
                    Some(Han::Hk)
                } else if rest.contains(&"hant") || rest.contains(&"tw") {
                    Some(Han::Tc)
                } else {
                    Some(Han::Sc)
                }
            }
            _ => None,
        }
    }

    /// The process's locale (`LC_ALL`, `LC_CTYPE`, `LANG`), read once.
    pub fn of_system() -> Option<Han> {
        static H: OnceLock<Option<Han>> = OnceLock::new();
        *H.get_or_init(|| {
            ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty())).and_then(|s| Han::of_locale(&s))
        })
    }

    /// For `text`: its own hint, else the locale's.
    pub fn for_text(text: &str) -> Option<Han> {
        Han::of_text(text).or_else(Han::of_system)
    }

    /// Whether `family` writes this convention.
    pub fn writes(self, family: &str) -> bool {
        family.rsplit(' ').next() == Some(self.suffix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        assert_eq!(Han::of_text("東京の天気"), Some(Han::Jp));
        assert_eq!(Han::of_text("한국어 漢字"), Some(Han::Kr));
        assert_eq!(Han::of_text("中文 text"), None);
        assert_eq!(Han::of_locale("ja_JP.UTF-8"), Some(Han::Jp));
        assert_eq!(Han::of_locale("zh_TW.UTF-8"), Some(Han::Tc));
        assert_eq!(Han::of_locale("zh-Hant"), Some(Han::Tc));
        assert_eq!(Han::of_locale("zh_HK"), Some(Han::Hk));
        assert_eq!(Han::of_locale("zh_CN.UTF-8"), Some(Han::Sc));
        assert_eq!(Han::of_locale("ko_KR"), Some(Han::Kr));
        assert_eq!(Han::of_locale("en_US.UTF-8"), None);
        assert_eq!(Han::of_locale("C"), None);
        assert!(Han::Jp.writes("Noto Sans JP"));
        assert!(!Han::Jp.writes("Noto Sans KR"));
    }
}
