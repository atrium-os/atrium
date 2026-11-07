//! A ready host fed an arbitrary sequence of controller frames (events and
//! ACL, as a hostile controller or a hostile peer's data would produce).
//! Must never panic, and must keep its bounds.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_host::testing::ready_host;
use vinculum_host::MAX_CONNECTIONS;

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
        while h.poll_event().is_some() {}
        assert!(h.connections() <= MAX_CONNECTIONS);
        if rest.first() == Some(&0xEE) {
            let _ = h.send_l2cap(0, 4, &[1, 2, 3], now);
        }
    }
});
