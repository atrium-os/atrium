//! Any HTTP response from a gateway (vpn.md §9.4): the head, body and
//! CONNECT push readers never panic, and reading byte by byte as the TLS
//! stream gives it agrees with reading it whole.
//!   cargo fuzz run http
#![no_main]
use crypta_ac::{cstp, http};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&cut, data)) = data.split_first() else { return };
    let whole = http::head(data);
    // A prefix is either not enough yet or the same answer.
    let part = &data[..(cut as usize).min(data.len())];
    match (http::head(part), &whole) {
        (Ok(None), _) => {}
        (Ok(Some(p)), Ok(Some(w))) => assert_eq!(&p, w),
        (Err(_), Err(_)) => {}
        (p, w) => panic!("prefix {p:?} whole {w:?}"),
    }
    let Ok(Some((h, used))) = whole else { return };
    assert!(used <= data.len());
    let _ = cstp::push(&h);
    let rest = &data[used..];
    let all = http::body(&h, rest);
    if let Ok(Some((b, n))) = &all {
        assert!(*n <= rest.len() && b.len() <= rest.len());
    }
    let part = &rest[..(cut as usize).min(rest.len())];
    match (http::body(&h, part), &all) {
        (Ok(None), _) => {}
        (Ok(Some(p)), Ok(Some(w))) => assert_eq!(&p, w),
        (Err(_), Err(_)) => {}
        (p, w) => panic!("prefix {p:?} whole {w:?}"),
    }
});
