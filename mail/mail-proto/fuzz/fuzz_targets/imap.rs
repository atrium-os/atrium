#![no_main]
//! Server bytes in any split: the decoder never panics, and stops on the
//! first error (the connection is dropped there).
use libfuzzer_sys::fuzz_target;
use mail_proto::imap::{Decoder, Limits};

fuzz_target!(|data: &[u8]| {
    let Some((&split, rest)) = data.split_first() else { return };
    let mut d = Decoder::new(Limits { max_literal: 1 << 16, max_response: 1 << 18, max_depth: 32, max_items: 4096 });
    for chunk in rest.chunks(split as usize + 1) {
        d.feed(chunk);
        loop {
            match d.next() {
                Some(Ok(_)) => continue,
                Some(Err(_)) => return,
                None => break,
            }
        }
    }
});
