//! The SDK's `atrium_assets::image` against this decoder, run as a plain
//! child (`ATRIUM_WORKERS=local`): the same wire and checks as on Atrium,
//! without the jail.

#[test]
fn the_sdk_decodes_a_bundle_picture_through_it() {
    let dir = std::env::temp_dir().join(format!("image-decode-local-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("assets/icons")).unwrap();
    std::fs::write(dir.join("assets/icons/app.svg"),
        br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><circle cx="5" cy="5" r="5" fill="#2050a0"/></svg>"##).unwrap();
    std::fs::write(dir.join("assets/junk.png"), b"\x89PNG\r\n\x1a\nnot really").unwrap();
    std::env::set_var("ATRIUM_APP_ROOT", &dir);
    std::env::set_var("ATRIUM_WORKERS", "local");
    std::env::set_var("ATRIUM_DECODER_IMAGE", env!("CARGO_BIN_EXE_atrium-image-decode"));
    let p = atrium_assets::image("icons/app.svg", 32, 20).unwrap();
    // A square icon fitted into 32×20 is drawn at 20×20.
    assert_eq!((p.width, p.height), (20, 20));
    let at = |x: usize, y: usize| { let i = (y * 20 + x) * 4; p.rgba[i..i + 4].to_vec() };
    assert_eq!(at(10, 10), [0x20, 0x50, 0xa0, 0xff]);
    assert_eq!(at(0, 0)[3], 0);
    // A broken picture is the decoder's error, not a crash here.
    match atrium_assets::image("junk.png", 16, 16) {
        Err(atrium_assets::Error::Decoder(atrium_worker::Error::Worker(why))) => assert!(!why.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(matches!(atrium_assets::image("missing.png", 16, 16), Err(atrium_assets::Error::Open(_))));
    let _ = std::fs::remove_dir_all(&dir);
}
