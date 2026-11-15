#![no_main]
//! Any bytes: parse never panics, every kept string respects its cap, and
//! every part's body range lies inside the message.
use libfuzzer_sys::fuzz_target;
use mail_parse::{parse, part_bytes, Limits};

fuzz_target!(|data: &[u8]| {
    let l = Limits { max_message: 1 << 20, max_text: 4096, ..Limits::default() };
    if let Ok(s) = parse(data, &l) {
        assert!(s.subject.len() <= l.max_short);
        assert!(s.preview.len() <= l.max_preview);
        assert!(s.text.as_ref().map_or(true, |t| t.len() <= l.max_text));
        assert!(s.html.as_ref().map_or(true, |t| t.len() <= l.max_text));
        assert!(s.parts.len() <= l.max_parts);
        for p in &s.parts {
            assert!(p.body.0 <= p.body.1 && p.body.1 <= data.len());
            let _ = part_bytes(data, p);
        }
        for a in s.from.iter().chain(&s.to).chain(&s.cc) {
            assert!(!a.name.chars().any(|c| c.is_control() || mail_parse::header::is_invisible(c)));
        }
    }
});
