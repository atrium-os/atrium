//! SDP responses from a paired (or merely connected) Classic device: the
//! first byte picks how many response PDUs the input is split into, so
//! continuation handling is reached too.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_sdp::{hid_descriptor, parse, text, uint, Query, uuid, attr};

fuzz_target!(|data: &[u8]| {
    let _ = parse(data);
    let Some((&n, rest)) = data.split_first() else { return };
    let parts = (n as usize % 8) + 1;
    let step = rest.len() / parts + 1;
    let (mut q, _) = Query::new(uuid::HID, &[attr::HID_DESCRIPTOR_LIST, attr::SERVICE_NAME]);
    for chunk in rest.chunks(step) {
        match q.on_response(chunk) {
            Ok(Ok(records)) => {
                for r in &records {
                    let _ = hid_descriptor(r);
                    let _ = text(r, attr::SERVICE_NAME);
                    let _ = uint(r, attr::VENDOR_ID);
                }
                return;
            }
            Ok(Err(_next)) => {}
            Err(_) => return,
        }
    }
});
