//! The DASH manifest scan for `pssh` boxes (demux/src/dash.rs): untrusted text from the page.
#![no_main]
use atrium_media_demux::dash;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for b in dash::pssh_boxes(data, dash::WIDEVINE) {
        assert!(b.len() <= dash::PSSH_MAX && &b[4..8] == b"pssh");
    }
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = dash::pssh_from_base64(s, dash::COMMON);
    }
});
