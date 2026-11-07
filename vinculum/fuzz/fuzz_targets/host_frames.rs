//! A ready host fed an arbitrary sequence of controller frames (events and
//! ACL, as a hostile controller or a hostile peer's data would produce).
//! Must never panic, and must keep its bounds.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_host::testing::ready_host;
use vinculum_host::{HostEvent, PairConfig, MAX_CONNECTIONS};
use vinculum_smp::pairing::IoCap;

fuzz_target!(|data: &[u8]| {
    let mut h = ready_host(2);
    let mut rest = data;
    let mut now = 0u64;
    // Records: kind selector, length, bytes.
    while rest.len() >= 2 {
        let kind = if rest[0] & 1 == 0 { Kind::Event } else { Kind::Acl };
        let n = (rest[1] as usize).min(rest.len() - 2);
        let packet = rest[2..2 + n].to_vec();
        rest = &rest[2 + n..];
        h.on_frame(Frame { kind, packet }, now);
        now += 10;
        h.tick(now);
        while h.poll_frame().is_some() {}
        while let Some(e) = h.poll_event() {
            // Drive security too: pair every new link, say yes to prompts.
            match e {
                HostEvent::Connected { handle, .. } => {
                    let _ = h.pair(handle, PairConfig { io_cap: IoCap::KeyboardDisplay, irk: [1; 16] }, now);
                }
                HostEvent::PairingPrompt { handle, .. } => h.pairing_confirm(handle, true, now),
                _ => {}
            }
        }
        assert!(h.connections() <= MAX_CONNECTIONS);
        if rest.first() == Some(&0xEE) {
            let _ = h.send_l2cap(0, 4, &[1, 2, 3], now);
        }
    }
});
