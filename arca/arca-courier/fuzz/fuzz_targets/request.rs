//! The courier's request schema against hostile clients (arca.md §5.1):
//! decoding never panics or over-allocates, and anything accepted has one
//! encoding (so what the courier acts on is exactly what was sent).
//!   cargo fuzz run request
#![no_main]
use arca_courier::schema::Request;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(r) = Request::decode(data) {
        assert_eq!(r.encode(), data);
        assert!(r.slots.len() <= arca_courier::schema::SLOTS_MAX);
    }
});
