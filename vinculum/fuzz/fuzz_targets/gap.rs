//! Advertising data from anyone in radio range.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_hci_proto::gap;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = gap::parse(data) {
        let _ = gap::local_name(&s);
    }
});
