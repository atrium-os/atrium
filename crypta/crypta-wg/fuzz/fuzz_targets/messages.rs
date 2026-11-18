//! Any datagram from the internet (vpn.md §4.2): parsing never panics, and
//! a handshake message parsed encodes back to the same bytes.
//!   cargo fuzz run messages
#![no_main]
use crypta_wg::messages::{parse, Message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match parse(data) {
        Some(Message::Initiation(i)) => assert_eq!(&i.encode()[..], data),
        Some(Message::Response(r)) => assert_eq!(&r.encode()[..], data),
        Some(Message::CookieReply(c)) => assert_eq!(&c.encode()[..], data),
        Some(Message::Transport(t)) => {
            assert_eq!(data[0], 4);
            assert_eq!(t.sealed.len(), data.len() - 16);
        }
        None => {}
    }
});
