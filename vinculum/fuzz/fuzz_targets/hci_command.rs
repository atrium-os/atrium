//! HCI commands as the transport's allowlist (spec §5.3) will see them from a
//! possibly compromised host: decode and specialize, never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_hci_proto::hci::Command;
use vinculum_hci_proto::Packet;

fuzz_target!(|data: &[u8]| {
    if let Ok(c) = Command::decode_full(data) {
        let _ = c.specialize();
    }
});
