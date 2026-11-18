//! The CJK font packages built by scripts/opifex-cjk-fonts.sh, installed as
//! opifexd installs a font and read back by atrium-fonts (fonts.md §5). Runs
//! only when OPIFEX_CJK_DIR names that script's output and OPIFEX_CJK_PUB the
//! public key it was signed with:
//!
//!   sh scripts/opifex-cjk-fonts.sh /tmp/cjk --sign platform.pem
//!   OPIFEX_CJK_DIR=/tmp/cjk OPIFEX_CJK_PUB=platform.pub.pem cargo test -p opifex --test cjk_fonts

use atrium_fonts::{Han, Library};

#[test]
fn cjk_packages_install_and_draw_their_forms() {
    let (Some(dir), Some(pubp)) = (std::env::var_os("OPIFEX_CJK_DIR"), std::env::var_os("OPIFEX_CJK_PUB")) else {
        eprintln!("SKIP: OPIFEX_CJK_DIR and OPIFEX_CJK_PUB not set");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let key = std::fs::read_to_string(pubp).unwrap();
    let fonts = std::env::temp_dir().join(format!("opifex-cjk-{}", std::process::id()));
    std::fs::create_dir_all(&fonts).unwrap();
    // SC first, so install order alone would pick SC: the hints must win.
    for l in ["sc", "jp", "tc", "hk", "kr"] {
        let (m, _) = opifex::font::install(&dir.join(format!("org.atrium.font.noto-sans-{l}")), &fonts, &[key.clone()])
            .unwrap_or_else(|e| panic!("{l}: {e}"));
        assert_eq!(m.faces.len(), 2);
    }
    std::env::set_var("ATRIUM_FONTS_EXTRA_DIR", &fonts);
    for (han, family) in [(Han::Sc, "Noto Sans SC"), (Han::Jp, "Noto Sans JP"), (Han::Tc, "Noto Sans TC"),
                          (Han::Hk, "Noto Sans HK"), (Han::Kr, "Noto Sans KR")] {
        assert_eq!(Library::fallback_in('直', Some(han)).next(), Some(family), "{family} first for 直 with its hint");
        let ids: Vec<usize> = Library::family(family).collect();
        assert_eq!(ids.len(), 2, "{family}: regular and bold");
        for id in ids {
            assert!(Library::system().bytes(id).unwrap().len() > 1_000_000, "{family} face {id} maps");
        }
    }
    assert_eq!(Library::fallback_in('あ', None).next(), Some("Noto Sans JP"), "kana: only JP has it");
    assert_eq!(Library::fallback_in('한', None).next(), Some("Noto Sans KR"), "Hangul: only KR has it");
    let _ = std::fs::remove_dir_all(&fonts);
}
