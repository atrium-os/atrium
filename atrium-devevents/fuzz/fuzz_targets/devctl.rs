//! devd lines and `devinfo -v` text into the typed parser, the registry and
//! the HID classifier: no panic, the registry stays bounded.
#![no_main]
use atrium_devevents::devctl::{devinfo_events, parse_line, Registry, MAX_DEVICES};
use atrium_devevents::hidowner::Owners;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let mut r = Registry::default();
    let mut o = Owners::default();
    for e in devinfo_events(&s) {
        r.feed(&e);
    }
    o.devinfo(&s);
    for line in s.split('\n') {
        if let Some(e) = parse_line(line) {
            r.feed(&e);
        }
        o.line(line);
    }
    assert!(r.len() <= MAX_DEVICES);
});
