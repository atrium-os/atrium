#![no_main]
//! The mail relay filter, both protocols, with the input cut into client and
//! server chunks however the fuzzer likes: it never panics, holds no more
//! than its cap, and what it lets through to the server is always exactly a
//! prefix of what the client sent (it never invents or reorders bytes).
use arca_courier::mail_filter::{Imap, Smtp, Stop, MAX_HELD};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else { return };
    let mut imap = Imap::new();
    let mut smtp = Smtp::new();
    let mut sent_by_client = Vec::new();
    let mut to_server = Vec::new();
    // Chunks: a length byte whose top bit says which side, then the bytes.
    let mut r = rest;
    while let Some((&h, tail)) = r.split_first() {
        let n = ((h & 0x7f) as usize).min(tail.len());
        let (chunk, next) = tail.split_at(n);
        r = next;
        let res: Result<(), Stop> = if h & 0x80 == 0 {
            sent_by_client.extend_from_slice(chunk);
            if mode & 1 == 0 { imap.client(chunk, &mut to_server) } else { smtp.client(chunk, &mut to_server) }
        } else if mode & 1 == 0 {
            imap.server(chunk, &mut to_server)
        } else {
            smtp.server(chunk, &mut to_server)
        };
        assert!(sent_by_client.starts_with(&to_server), "forwarded bytes are not a prefix of the client's");
        assert!(sent_by_client.len() - to_server.len() <= MAX_HELD + 64 * 1024 + 4096);
        if res.is_err() { return }
    }
});
