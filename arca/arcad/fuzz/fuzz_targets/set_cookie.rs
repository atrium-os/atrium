//! Set-Cookie from a hostile server (arca.md §8.1): parsing never panics,
//! and whatever is accepted obeys the rules (domain on the host's side of
//! the public suffix, prefixes, capped lifetime, a scope that round-trips).
//!   cargo fuzz run set_cookie
#![no_main]
use arcad::cookie::{self, Attrs};
use libfuzzer_sys::fuzz_target;

const HOSTS: [&str; 4] = ["login.example.com", "example.com", "me.github.io", "a.b.example.co.uk"];
const NOW: u64 = 1_800_000_000;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let host = HOSTS[data[0] as usize % HOSTS.len()];
    let rest = &data[1..];
    let (head, attrs) = rest.split_at(rest.iter().position(|&b| b == b';').unwrap_or(rest.len()));
    let Some(eq) = head.iter().position(|&b| b == b'=') else { return };
    let Ok(name) = std::str::from_utf8(&head[..eq]) else { return };
    let _ = cookie::parse_date(std::str::from_utf8(attrs).unwrap_or(""));
    if let Ok(sc) = cookie::parse(name, &head[eq + 1..], attrs, host, "/a/b", NOW) {
        let d = &sc.domain;
        assert!(host == d || host.ends_with(&format!(".{d}")));
        assert!(sc.attrs.host_only || !cookie::is_public_suffix(d));
        assert!(sc.attrs.host_only == (d == host) || !sc.attrs.host_only);
        assert!(sc.attrs.expires.is_none_or(|e| e <= NOW + cookie::MAX_AGE_CAP));
        assert!(sc.attrs.path.starts_with('/'));
        if sc.name.starts_with("__Host-") {
            assert!(sc.attrs.secure && sc.attrs.host_only && sc.attrs.path == "/");
        }
        assert_eq!(Attrs::decode(&sc.attrs.encode()), Some(sc.attrs.clone()));
        // It matches its own host at its own path while it lives.
        if !sc.expired(NOW) && sc.attrs.host_only {
            assert!(cookie::applies(d, &sc.attrs, host, &sc.attrs.path, NOW));
        }
    }
    let _ = Attrs::decode(rest);
});
