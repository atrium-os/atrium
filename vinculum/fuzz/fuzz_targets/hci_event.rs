//! An HCI event from a hostile controller (or carrying hostile radio data):
//! decode and specialize every level the host would. Must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_hci_proto::hci::{Event, EventChild};
use vinculum_hci_proto::{gap, Packet};

fuzz_target!(|data: &[u8]| {
    let Ok(e) = Event::decode_full(data) else { return };
    match e.specialize() {
        Ok(EventChild::CommandComplete(cc)) => {
            let _ = cc.specialize();
        }
        Ok(EventChild::CommandStatus(cs)) => {
            let _ = cs.specialize();
        }
        Ok(EventChild::VendorSpecificEvent(v)) => {
            let _ = v.specialize();
        }
        Ok(EventChild::LeMetaEvent(m)) => {
            use vinculum_hci_proto::hci::LeMetaEventChild as L;
            match m.specialize() {
                Ok(L::LeAdvertisingReport(r)) => {
                    for x in r.responses {
                        let _ = gap::parse(&x.advertising_data);
                    }
                }
                Ok(L::LeExtendedAdvertisingReport(r)) => {
                    for x in r.responses {
                        let _ = gap::parse(&x.advertising_data);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
});
