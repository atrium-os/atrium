//! Nuntius's wire (navigator-islands.md I6.6): a request from an engine
//! and an event from a fetcher are both hostile. None may panic, and what
//! is taken re-encodes to something that reads the same.
#![no_main]
use libfuzzer_sys::fuzz_target;
use nuntius::wire::WireEvent;
use nuntius::Request;

fuzz_target!(|data: &[u8]| {
    if let Some(r) = Request::decode(data) {
        assert!(r.valid());
        assert_eq!(Request::decode(&r.encode()), Some(r));
    }
    if let Some(e) = WireEvent::decode(data) {
        assert_eq!(WireEvent::decode(&e.encode()), Some(e));
    }
});
