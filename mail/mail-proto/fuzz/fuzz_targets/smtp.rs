#![no_main]
use libfuzzer_sys::fuzz_target;
use mail_proto::smtp::{Caps, ReplyDecoder, Step, Submission};

fuzz_target!(|data: &[u8]| {
    let mut d = ReplyDecoder::default();
    d.feed(data);
    let caps = Caps::default();
    let mut s = Submission::new(&caps, "a@b.example", &["c@d.example", "e@f.example"], b"x", None).unwrap();
    s.start();
    while let Some(Ok(r)) = d.next() {
        let _ = Caps::from_ehlo(&r);
        if !matches!(s.on_reply(r), Step::Send(_)) {
            break;
        }
    }
});
